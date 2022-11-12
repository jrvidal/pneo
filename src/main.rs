use api::Metadata;
use async_std::task::JoinHandle;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{
    future::{pending, OptionFuture},
    stream::BoxStream,
    FutureExt, Stream, StreamExt,
};
use std::{io, panic::AssertUnwindSafe, time::Duration};
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    widgets::{Block, Borders, List, ListItem},
    Terminal,
};

use crate::api::Response;

mod api;

fn main() -> Result<(), io::Error> {
    {
        let mut builder = env_logger::Builder::from_default_env();

        builder.target(env_logger::Target::Pipe(Box::new(
            std::fs::File::create("pneo.log").unwrap(),
        )));

        builder.init();
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let assert = AssertUnwindSafe(&mut terminal);
    let result = std::panic::catch_unwind(|| {
        let mut assert = assert;
        main_loop(&mut assert.0)
    });

    disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    match result {
        Ok(result) => result,
        Err(any) => {
            eprintln!("main loop panicked");

            if let Some(s) = any.downcast_ref::<&str>() {
                eprintln!("{}", s);
            }

            if let Some(s) = any.downcast_ref::<String>() {
                eprintln!("{}", s);
            }

            std::process::exit(1);
        }
    }
}

struct State {
    input: String,
    output: Option<surf::Result<Vec<Metadata>>>,
    spinner: u8,
    focus: u8,
}

fn main_loop<B: tui::backend::Backend>(terminal: &mut Terminal<B>) -> Result<(), io::Error> {
    let mut state = State {
        input: String::new(),
        output: None,
        spinner: 0,
        focus: 0,
    };
    let mut request: Option<JoinHandle<surf::Result<Response>>> = None;
    let mut spinner: Option<BoxStream<()>> = None;

    loop {
        ui(terminal, &state)?;

        let mut stream = EventStream::new();

        enum Message {
            Event(Option<Result<Event, io::Error>>),
            Response(surf::Result<Response>),
            Spin,
        }

        let mut next_event = stream.next().fuse();
        let spin = async {
            match &mut spinner {
                Some(spinner) => spinner.next().await,
                None => pending().await,
            }
        }
        .fuse();

        let message = async_std::task::block_on(async {
            let req = OptionFuture::from(request.as_mut())
                .then(|res| async move {
                    match res {
                        Some(res) => futures::future::ready(res).await,
                        None => pending().await,
                    }
                })
                .fuse();

            futures::pin_mut!(req);
            futures::pin_mut!(spin);

            futures::select! {
                ev = next_event => Message::Event(ev),
                res = req => Message::Response(res),
                _ = spin => Message::Spin,
            }
        });

        log::debug!("{:?}", std::matches!(message, Message::Event(..)));
        match message {
            Message::Event(None) => return Ok(()),
            Message::Event(Some(ev)) => {
                let changed = match ev? {
                    Event::Key(key) => match key.code {
                        KeyCode::Esc => return Ok(()),
                        KeyCode::Char(ch) => {
                            state.input.push(ch);
                            true
                        }
                        KeyCode::Enter => {
                            state.input.push('\n');
                            true
                        }
                        KeyCode::Delete | KeyCode::Backspace => {
                            state.input.pop();
                            true
                        }
                        KeyCode::Down => {
                            if let Some(Ok(entries)) = &state.output {
                                if !entries.is_empty() {
                                    state.focus = (state.focus + 1).min((entries.len() - 1) as u8);
                                }
                            }
                            false
                        }
                        KeyCode::Up => {
                            if let Some(Ok(_)) = &state.output {
                                state.focus = state.focus.saturating_sub(1);
                            }
                            false
                        }
                        _ => continue,
                    },
                    _ => continue,
                };

                if !changed {
                    continue;
                }

                if let Some(request) = request.take() {
                    let _ = request.cancel();
                }

                spinner = None;

                if state.input.len() < 3 {
                    continue;
                }

                log::info!("requesting with {:?}", &state.input);
                request = Some(async_std::task::spawn(api::get(state.input.clone())));
                spinner = Some(Box::pin(spinner_stream()));
            }
            Message::Response(res) => {
                state.output = Some(
                    res.map(|res| res.hits.hits.into_iter().map(|hit| hit.metadata).collect()),
                );
                request = None;
                spinner = None;
                state.spinner = 0;
                state.focus = 0;
            }
            Message::Spin => {
                state.spinner += 1;

                if state.spinner % 4 == 1 {
                    state.spinner = 1;
                }
            }
        }
    }
}

fn spinner_stream() -> impl futures::Stream<Item = ()> {
    futures::stream::repeat(()).then(|_| async_std::task::sleep(Duration::from_millis(100)))
}

fn ui<'t, 's, B: tui::backend::Backend>(
    terminal: &'t mut Terminal<B>,
    state: &'s State,
) -> Result<tui::terminal::CompletedFrame<'t>, io::Error> {
    terminal.draw(|f| {
        let size = f.size();
        let block = Block::default().borders(Borders::ALL);
        f.render_widget(block, size);

        let input_block = Block::default().borders(Borders::ALL);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
            .split(size);

        let input_chunk = chunks[0];
        let results_chunk = chunks[1];
        f.render_widget(input_block, input_chunk);

        let mut text_chunk = input_chunk;
        text_chunk.y += 1;
        text_chunk.height -= 1;
        text_chunk.x += 5;
        text_chunk.width -= 5;
        f.render_widget(Block::default().title(state.input.clone()), text_chunk);

        let icon = ["|", "/", "-", "\\"][(state.spinner % 4) as usize];
        text_chunk.x -= 3;
        text_chunk.width = 4;
        f.render_widget(
            Block::default().title(if state.spinner == 0 { " " } else { icon }),
            text_chunk,
        );

        let message_chunk = Layout::default()
            .margin(1)
            .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
            .split(results_chunk)[0];

        // let mut below = input_chunk;
        // below.y = input_chunk.y + input_chunk.height + 5;

        let results = match &state.output {
            None => None,
            Some(Ok(res)) => {
                if res.is_empty() {
                    f.render_widget(
                        Block::default().title("No results".to_string()),
                        message_chunk,
                    );
                    None
                } else {
                    Some(res)
                }
            }
            Some(Err(err)) => {
                f.render_widget(Block::default().title(format!("{:?}", err)), message_chunk);
                None
            }
        };

        if let Some(results) = results {
            let items = results
                .iter()
                .map(|metadata| metadata.title().unwrap_or("(No title)"))
                .enumerate()
                .map(|(i, title)| {
                    ListItem::new(format!(
                        "{} {}",
                        if i == state.focus as usize { ">" } else { " " },
                        title
                    ))
                })
                .collect::<Vec<_>>();

            f.render_widget(List::new(items), results_chunk);
        }
    })
}
