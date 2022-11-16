use anyhow::Context;
use api::Metadata;

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{
    future::{Fuse, FusedFuture, OptionFuture},
    stream::{self, FusedStream},
    Future, FutureExt, StreamExt,
};
use std::{
    io::{self, Write},
    panic::AssertUnwindSafe,
    path::PathBuf,
    pin::Pin,
    process::Command,
    task::Poll,
    time::Duration,
};
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    widgets::{Block, Borders, List, ListItem},
    Terminal,
};

use crate::api::{ArxivSearchResult, InspiresSearchResult};

mod api;

fn main() -> anyhow::Result<()> {
    let dir = {
        let mut dir = dirs::config_dir().ok_or(io::Error::new(
            io::ErrorKind::Other,
            "unable to find config directory",
        ))?;

        dir.push("pneo");

        dir
    };

    let cache = {
        let mut cache = dir.clone();
        cache.push("cache");
        cache
    };

    std::fs::create_dir_all(&dir)
        .context(format!("Unable to create config directory at {:?}", dir))?;
    std::fs::create_dir_all(&cache)
        .context(format!("Unable to create cache directory at {:?}", cache))?;

    {
        let mut builder = env_logger::Builder::from_default_env();
        let logfile = {
            let mut logfile = dir.clone();
            logfile.push("pneo.log");
            logfile
        };

        builder
            .filter_level(log::LevelFilter::Warn)
            .target(env_logger::Target::Pipe(Box::new(
                std::fs::File::create(logfile).context("Unable to create logfile")?,
            )));

        builder.init();
    }

    fn start_terminal() -> io::Result<Terminal<CrosstermBackend<impl Write>>> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(terminal)
    }

    let mut terminal = match start_terminal().context("unable to start terminal interface") {
        Err(e) => {
            log::debug!("{:?}", e);
            Err(e)
        }
        res => res,
    }?;

    let assert = AssertUnwindSafe(&mut terminal);
    let result = std::panic::catch_unwind(|| {
        let mut assert = assert;
        main_loop(&mut assert.0, cache)
    });

    fn stop_terminal(mut terminal: Terminal<CrosstermBackend<impl Write>>) -> io::Result<()> {
        disable_raw_mode()?;
        crossterm::execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;
        Ok(())
    }

    if let Err(e) = stop_terminal(terminal).context("unable to restore terminal") {
        log::debug!("{:?}", e);
    }

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
    searching: bool,
    fetching: bool,
    focus: u8,
}

impl State {
    fn busy(&self) -> bool {
        self.searching || self.fetching
    }
}

fn main_loop<B: tui::backend::Backend>(
    terminal: &mut Terminal<B>,
    cache: PathBuf,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .context("unable to start runtime")?;

    let _guard = runtime.enter();

    let mut state = State {
        input: String::new(),
        output: None,
        searching: false,
        fetching: false,
        focus: 0,
    };

    let mut spinner_state = SpinnerState::new();

    let mut draw = true;

    let search_request = Fuse::terminated();
    let preprint_request = Fuse::terminated();
    let download_request = Fuse::terminated();
    let mut event_stream = EventStream::new().fuse();
    let throttle = Fuse::terminated();
    futures::pin_mut!(search_request);
    futures::pin_mut!(preprint_request);
    futures::pin_mut!(download_request);
    futures::pin_mut!(throttle);
    loop {
        spinner_state.spin(state.busy());
        if draw {
            log::debug!("drawing!");
            ui(terminal, &state, &spinner_state)?;
        }

        draw = true;

        enum Message {
            Event(Option<Result<Event, io::Error>>),
            SearchResponse(surf::Result<InspiresSearchResult>),
            PreprintResponse(surf::Result<ArxivSearchResult>),
            Downloaded(PathBuf),
            Spin,
            Commit,
        }

        #[derive(Debug)]
        enum MessageDebug<'a> {
            Event(Option<&'a Event>),
            SearchResponse(&'a surf::Result<InspiresSearchResult>),
            PreprintResponse(&'a surf::Result<ArxivSearchResult>),
            Downloaded(&'a PathBuf),
            Spin,
            Commit,
        }

        impl<'a> From<&'a Message> for MessageDebug<'a> {
            fn from(message: &'a Message) -> Self {
                match message {
                    Message::Event(event) => {
                        MessageDebug::Event(event.as_ref().and_then(|ev| ev.as_ref().ok()))
                    }
                    Message::SearchResponse(res) => MessageDebug::SearchResponse(res),
                    Message::PreprintResponse(res) => MessageDebug::PreprintResponse(res),
                    Message::Downloaded(res) => MessageDebug::Downloaded(res),
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
        let message = runtime.block_on(async {
            let mut spin = spinner_state.stream();

            futures::select! {
                ev =  event_stream.next() => Message::Event(ev),
                res =  &mut search_request => Message::SearchResponse(res),
                _ =  spin.next() => Message::Spin,
                _ =  &mut throttle => Message::Commit,
                preprint = &mut preprint_request => Message::PreprintResponse(preprint),
                download = &mut download_request => Message::Downloaded(download),
            }
        });

        log::debug!("message = {:?}", message);

        match message {
            Message::Event(None) => return Ok(()),
            Message::Event(Some(ev)) => {
                draw = false;
                enum Action {
                    Input,
                    Select,
                }

                let action = match ev? {
                    Event::Key(key) => match key.code {
                        KeyCode::Esc => return Ok(()),
                        KeyCode::Char(ch) => {
                            if !state.fetching {
                                state.input.push(ch);
                                Some(Action::Input)
                            } else {
                                continue;
                            }
                        }
                        KeyCode::Enter => {
                            if state.busy() {
                                continue;
                            } else {
                                Some(Action::Select)
                            }
                        }
                        KeyCode::Delete | KeyCode::Backspace => {
                            state.input.pop();
                            Some(Action::Input)
                        }
                        KeyCode::Down => {
                            if state.busy() {
                                continue;
                            }

                            if let Some(Ok(entries)) = &state.output {
                                if !entries.is_empty() {
                                    state.focus = (state.focus + 1).min((entries.len() - 1) as u8);
                                }
                            }
                            None
                        }
                        KeyCode::Up => {
                            if state.busy() {
                                continue;
                            }

                            if let Some(Ok(_)) = &state.output {
                                state.focus = state.focus.saturating_sub(1);
                            }
                            None
                        }
                        _ => continue,
                    },
                    _ => continue,
                };

                draw = true;

                let Some(action) = action else {
                    continue;
                };

                match action {
                    Action::Input => {
                        search_request.set(Fuse::terminated());
                        throttle.set(tick(400).fuse());
                        state.searching = true;
                    }
                    Action::Select => {
                        let preprint_id = state
                            .output
                            .as_ref()
                            .and_then(|res| res.as_ref().ok())
                            .and_then(|entries| entries.get(state.focus as usize))
                            .and_then(|entry| entry.eprint());

                        log::debug!("Preprint {:?}", preprint_id);
                        if let Some(preprint_id) = preprint_id {
                            state.fetching = true;
                            preprint_request.set(api::get_preprint(preprint_id.to_string()).fuse());
                        }
                    }
                }
            }
            Message::SearchResponse(res) => {
                state.focus = 0;
                state.output = Some(
                    res.map(|res| res.hits.hits.into_iter().map(|hit| hit.metadata).collect()),
                );

                state.searching = false;
            }
            Message::PreprintResponse(res) => {
                let link = res
                    .ok()
                    .into_iter()
                    .flat_map(|result| result.entry.into_iter())
                    .flat_map(|entry| entry.link.into_iter())
                    .filter(|link| link.title.as_deref() == Some("pdf"))
                    .next();

                let Some(link) = link else {
                    state.fetching = false;
                    continue;
                };

                let url = link.href;
                let path = {
                    let mut path = cache.clone();
                    let path_id = url
                        .rsplit_once("/")
                        // FIXME: handle
                        .ok_or(anyhow::anyhow!("unexpected arxiv id {:?}", url))?
                        .1;
                    path.push(path_id);
                    path
                };

                if std::path::Path::new(&path).exists() {
                    state.fetching = false;
                    // FIXME: handle
                    Command::new("xdg-open").arg(path).spawn();
                    continue;
                }

                download_request.set(
                    surf::Client::new()
                        .with(surf::middleware::Redirect::default())
                        .get(url.clone())
                        .then(|res| {
                            OptionFuture::from(res.ok().map(|mut res| res.take_body().into_bytes()))
                        })
                        .map(|result| result.map(|res| res.ok()).flatten())
                        .map({
                            let path = path.clone();
                            move |bytes| {
                                bytes.into_iter().for_each(|content| {
                                    // FIXME: handle
                                    std::fs::write(&path, content);
                                })
                            }
                        })
                        .map(|_| path)
                        .fuse(),
                )
            }
            Message::Spin => {
                spinner_state.tick();
            }
            Message::Commit => {
                if state.input.len() < 3 {
                    state.searching = false;
                    continue;
                }

                log::info!("requesting with {:?}", &state.input);
                search_request.set(api::search_inspires(state.input.clone()).fuse());
            }
            Message::Downloaded(path) => {
                state.fetching = false;
                // FIXME: handle
                Command::new("xdg-open").arg(path).spawn();
            }
        }
    }
}

fn tick(millis: u64) -> impl Future<Output = ()> {
    tokio::time::sleep(Duration::from_millis(millis))
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

fn ui<'t, 's, B: tui::backend::Backend>(
    terminal: &'t mut Terminal<B>,
    state: &'s State,
    spinner: &'s SpinnerState,
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

        // let icon = ["|", "/", "-", "\\"][(state.spinner % 4) as usize];
        text_chunk.x -= 3;
        text_chunk.width = 4;
        f.render_widget(Block::default().title(spinner.icon()), text_chunk);

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
                    ListItem::new(format!(
                        "{} {}",
                        if !state.busy() && i == focus {
                            ">"
                        } else {
                            " "
                        },
                        title
                    ))
                })
                .collect::<Vec<_>>();

            f.render_widget(List::new(items), results_chunk);
        }
    })
}

struct SpinnerState {
    spinning: bool,
    frame: u8,
    stream: Pin<Box<dyn FusedStream<Item = ()>>>,
}

impl SpinnerState {
    fn new() -> Self {
        Self {
            spinning: false,
            frame: 0,
            stream: Box::pin(stream::empty()),
        }
    }

    fn spin(&mut self, spin: bool) {
        let was_spinning = self.spinning;
        self.spinning = spin;

        match (was_spinning, self.spinning) {
            (false, true) => {
                self.stream = Box::pin(ticks(45).fuse());
                self.frame = 0;
            }
            (true, false) => {
                self.stream = Box::pin(stream::empty());
            }
            _ => {}
        }
    }

    fn tick(&mut self) {
        self.frame += 1;
        self.frame %= 4;
    }

    fn stream(&mut self) -> impl FusedStream + '_ {
        &mut self.stream
    }

    fn icon(&self) -> &str {
        if !self.spinning {
            " "
        } else {
            ["|", "/", "-", "\\"][(self.frame % 4) as usize]
        }
    }
}
