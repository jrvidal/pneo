use api::Metadata;

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{
    future::{Fuse, FusedFuture},
    stream::{self, FusedStream},
    Future, FutureExt, StreamExt,
};
use std::{io, panic::AssertUnwindSafe, pin::Pin, task::Poll, time::Duration};
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    widgets::{Block, Borders, List, ListItem},
    Terminal,
};

use crate::api::InspiresSearchResult;

mod api;

fn main() -> Result<(), io::Error> {
    {
        let mut builder = env_logger::Builder::from_default_env();

        builder.target(env_logger::Target::Pipe(Box::new(
            std::fs::File::create("pneo.log").unwrap(),
        )));

        builder.init();
    }

    // async_std::task::block_on(async { api::get_preprint("hep-ph/0009092".to_string()).await })
    //     .unwrap();

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

    let request = Fuse::terminated();
    futures::pin_mut!(request);
    let mut draw = true;

    let mut event_stream = EventStream::new().fuse();

    let mut spinner_ticks: Pin<Box<dyn FusedStream<Item = ()>>> = Box::pin(stream::empty());
    let throttle = Fuse::terminated();
    futures::pin_mut!(throttle);

    loop {
        log::debug!("drawing!");
        if draw {
            ui(terminal, &state)?;
            log::debug!("drawed!");
        }

        draw = true;

        enum Message {
            Event(Option<Result<Event, io::Error>>),
            Response(surf::Result<InspiresSearchResult>),
            Spin,
            Commit,
        }

        #[derive(Debug)]
        enum MessageDebug<'a> {
            Event(Option<&'a Event>),
            Response(&'a surf::Result<InspiresSearchResult>),
            Spin,
            Commit,
        }

        impl<'a> From<&'a Message> for MessageDebug<'a> {
            fn from(message: &'a Message) -> Self {
                match message {
                    Message::Event(event) => {
                        MessageDebug::Event(event.as_ref().and_then(|ev| ev.as_ref().ok()))
                    }
                    Message::Response(res) => MessageDebug::Response(res),
                    Message::Spin => MessageDebug::Spin,
                    Message::Commit => MessageDebug::Commit,
                }
            }
        }

        impl std::fmt::Debug for Message {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                MessageDebug::from(self).fmt(f)
            }
        }

        log::debug!("next message...");
        let message = async_std::task::block_on(async {
            futures::select! {
                ev = InspectPoll{ fut: event_stream.next(), name: "event"} => Message::Event(ev),
                res = InspectPoll {fut: &mut request, name: "request" }=> Message::Response(res),
                _ = InspectPoll { fut: spinner_ticks.next() , name: "spin"} => Message::Spin,
                _ = InspectPoll {fut: &mut throttle, name: "commit"} => Message::Commit,
            }
        });

        log::debug!("message = {:?}", message);

        match message {
            Message::Event(None) => return Ok(()),
            Message::Event(Some(ev)) => {
                draw = false;
                let input_changed = match ev? {
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

                draw = true;

                if !input_changed {
                    continue;
                }

                request.set(Fuse::terminated());

                throttle.set(tick(400).fuse());
                spinner_ticks = Box::pin(ticks(75).fuse());
                // next_spin = Some(spinner_ticks.as_mut().unwrap().next()).into();
            }
            Message::Response(res) => {
                state.output = Some(
                    res.map(|res| res.hits.hits.into_iter().map(|hit| hit.metadata).collect()),
                );
                request.set(Fuse::terminated());

                spinner_ticks = Box::pin(stream::empty());
                state.spinner = 0;
                state.focus = 0;
            }
            Message::Spin => {
                // next_spin = Some(spinner_ticks.as_mut().unwrap().next()).into();
                state.spinner += 1;

                if state.spinner % 4 == 1 {
                    state.spinner = 1;
                }
            }
            Message::Commit => {
                throttle.set(Fuse::terminated());

                if state.input.len() < 3 {
                    // next_spin = None.into();
                    spinner_ticks = Box::pin(stream::empty());
                    continue;
                }

                log::info!("requesting with {:?}", &state.input);
                request.set(api::search_inspires(state.input.clone()).fuse());
            }
        }
    }
}

fn tick(millis: u64) -> impl Future<Output = ()> {
    async_std::task::sleep(Duration::from_millis(millis))
}

fn ticks(millis: u64) -> impl futures::Stream<Item = ()> {
    futures::stream::repeat(()).then(move |_| tick(millis))
}

struct InspectPoll<F> {
    fut: F,
    name: &'static str,
}

impl<F: Future> Future for InspectPoll<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let name = self.name;
        log::debug!("polling {:?}", name);
        let this = unsafe { self.get_unchecked_mut() };
        let poll = unsafe { Pin::new_unchecked(&mut this.fut) }.poll(cx);
        log::debug!("polled {:?} with ready = {}", name, poll.is_ready());
        poll
    }
}

impl<F: FusedFuture> FusedFuture for InspectPoll<F> {
    fn is_terminated(&self) -> bool {
        log::debug!(
            "is {:?} terminated? = {}",
            self.name,
            self.fut.is_terminated()
        );
        self.fut.is_terminated()
    }
}

struct MyFusedFuture<F> {
    fut: Option<F>,
}

impl<F> MyFusedFuture<F> {
    fn as_mut(&mut self) -> MyFusedFuture<&mut F> {
        MyFusedFuture {
            fut: self.fut.as_mut(),
        }
    }

    // unsafe fn pin_mut(&mut self) -> Pin<&mut Self> {
    //     unsafe { Pin::new_unchecked(&mut self) }
    // }

    fn as_pin_mut(self: Pin<&mut Self>) -> MyFusedFuture<Pin<&mut F>> {
        let this = unsafe { self.get_unchecked_mut() };
        let projection = unsafe { Pin::new_unchecked(&mut this.fut) };

        MyFusedFuture {
            fut: projection.as_pin_mut(),
        }

        // match this {
        //     Some(fut) => MyFusedFuture {
        //         fut: Some(unsafe { Pin::new_unchecked(fut) }),
        //     },
        //     None => MyFusedFuture { fut: None },
        // }
        // match &mut this.fut {
        //     Some(fut) => unsafe { Pin::new_unchecked(fut) },
        //     None => MyFusedFuture { fut: None },
        // };

        // if poll.is_ready() {
        //     this.fut = None;
        // }

        // poll
    }
}

impl<F> From<Option<F>> for MyFusedFuture<F> {
    fn from(fut: Option<F>) -> Self {
        MyFusedFuture { fut }
    }
}

impl<F: Future> Future for MyFusedFuture<F> {
    type Output = F::Output;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let poll = match &mut this.fut {
            Some(fut) => unsafe { Pin::new_unchecked(fut) }.poll(cx),
            None => Poll::Pending,
        };

        if poll.is_ready() {
            this.fut = None;
        }

        poll
    }
}

// impl<F: Unpin> Unpin for MyFusedFuture<F> {}

impl<F: Future> FusedFuture for MyFusedFuture<F> {
    fn is_terminated(&self) -> bool {
        self.fut.is_none()
    }
}

async fn maybe_stream<S: futures::Stream + Unpin>(
    stream: Option<S>,
) -> Option<<S as futures::Stream>::Item> {
    match stream {
        Some(mut stream) => stream.next().await,
        None => futures::future::pending().await,
    }
}

fn ui<'t, 's, B: tui::backend::Backend>(
    terminal: &'t mut Terminal<B>,
    state: &'s State,
) -> Result<tui::terminal::CompletedFrame<'t>, io::Error> {
    terminal.draw(|f| {
        let size = f.size();
        let block = Block::default().borders(Borders::ALL).title("πνέω");
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
            let space = results_chunk.height as usize;
            let mut focus = state.focus as usize;
            let mut offset = 0;

            if (focus + 1) > space {
                offset = focus + 1 - space;
                focus = space - 1;
            }

            let items = results
                .iter()
                .skip(offset)
                .take(space as usize)
                .map(|metadata| metadata.title().unwrap_or("(No title)"))
                .enumerate()
                .map(|(i, title)| {
                    ListItem::new(format!("{} {}", if i == focus { ">" } else { " " }, title))
                })
                .collect::<Vec<_>>();

            f.render_widget(List::new(items), results_chunk);
        }
    })
}
