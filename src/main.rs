use anyhow::{Context, Result};
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, MouseEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{
    future::{Fuse, FusedFuture},
    stream::{self, FusedStream},
    Future, FutureExt, StreamExt, TryFutureExt,
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
    layout::{Constraint, Direction, Layout, Margin},
    style::{Color, Modifier, Style},
    text::Span,
    widgets::{Block, Borders, Row, Table},
    Terminal,
};

use crate::api::{ArxivSearchResult, InspiresSearchResult, Metadata};

mod api;

fn main() -> anyhow::Result<()> {
    let data_dir = {
        let mut dir =
            dirs::data_dir().ok_or(anyhow::anyhow!("unable to find suitable data directory"))?;
        dir.push("pneo");
        dir
    };

    let runtime_dir = {
        let mut dir = dirs::runtime_dir()
            .ok_or(anyhow::anyhow!("unable to find suitable runtime directory"))?;
        dir.push("pneo");
        dir
    };

    let preprint_dir = {
        let mut dir = data_dir;
        dir.push("preprints");
        dir
    };

    std::fs::create_dir_all(&preprint_dir).context(format!(
        "Unable to create data directory at {:?}",
        preprint_dir
    ))?;
    std::fs::create_dir_all(&runtime_dir).context(format!(
        "Unable to create runtime directory at {:?}",
        runtime_dir
    ))?;

    {
        let mut builder = env_logger::Builder::from_default_env();
        let logfile = {
            let mut logfile = runtime_dir;
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

    if std::env::args_os().skip(1).next() == Some("--version".into()) {
        println!("pneo {}", std::env!("CARGO_PKG_VERSION"));
        return Ok(());
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
        main_loop(&mut assert.0, preprint_dir)
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
    cursor: usize,
    output: Option<surf::Result<TableState<Metadata>>>,
    searching: bool,
    fetching: bool,
    warning: Option<String>,
}

impl State {
    fn busy(&self) -> bool {
        self.searching || self.fetching
    }

    fn append(&mut self, ch: char) {
        if self.input.len() == self.cursor {
            self.input.push(ch);
        } else {
            self.input.insert(self.cursor, ch);
        }

        self.cursor += 1;
    }

    fn delete(&mut self) -> bool {
        if self.input.len() == self.cursor {
            if self.input.pop().is_some() {
                self.cursor -= 1;
                true
            } else {
                false
            }
        } else if self.cursor > 0 {
            self.input.remove(self.cursor - 1);
            self.cursor -= 1;
            true
        } else {
            false
        }
    }

    fn move_cursor(&mut self, step: i8) {
        let cursor = if step < 0 {
            self.cursor.saturating_sub(1)
        } else {
            (self.cursor + 1).min(self.input.len())
        };

        self.cursor = cursor;
    }

    fn down(&mut self, step: u8) -> bool {
        if self.busy() {
            return false;
        }

        let Some(Ok(table)) = &mut self.output else {
            return false;
        };

        table.down(step)
    }

    fn up(&mut self, step: u8) -> bool {
        if self.busy() {
            return false;
        }

        let Some(Ok(table)) = &mut self.output else {
            return false;
        };

        table.up(step)
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
        cursor: 0,
        output: None,
        searching: false,
        fetching: false,
        warning: None,
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
            ui(terminal, &mut state, &spinner_state)?;
        }

        draw = true;

        enum Message {
            Event(Option<io::Result<Event>>),
            SearchResponse(surf::Result<InspiresSearchResult>),
            PreprintResponse(surf::Result<ArxivSearchResult>),
            Downloaded(Result<PathBuf>),
            Spin,
            Commit,
        }

        #[derive(Debug)]
        enum MessageDebug<'a> {
            Event(Option<&'a Event>),
            SearchResponse(&'a surf::Result<InspiresSearchResult>),
            PreprintResponse(&'a surf::Result<ArxivSearchResult>),
            Downloaded(&'a Result<PathBuf>),
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
                    Event::Resize(..) => {
                        draw = true;
                        continue;
                    }
                    Event::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::ScrollDown => {
                            if state.down(1) {
                                None
                            } else {
                                continue;
                            }
                        }
                        MouseEventKind::ScrollUp => {
                            if state.up(1) {
                                None
                            } else {
                                continue;
                            }
                        }
                        _ => continue,
                    },
                    Event::Key(key) => match key.code {
                        KeyCode::Esc => {
                            if state.warning.take().is_none() {
                                return Ok(());
                            } else {
                                None
                            }
                        }
                        KeyCode::Char(ch) => {
                            if !state.fetching {
                                state.append(ch);
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
                            if !state.fetching && state.delete() {
                                Some(Action::Input)
                            } else {
                                continue;
                            }
                        }
                        key @ (KeyCode::Down | KeyCode::PageDown) => {
                            let step = if key == KeyCode::Down { 1 } else { 10 };

                            if state.down(step) {
                                None
                            } else {
                                continue;
                            }
                        }
                        key @ (KeyCode::Up | KeyCode::PageUp) => {
                            let step = if key == KeyCode::Up { 1 } else { 10 };
                            if state.up(step) {
                                None
                            } else {
                                continue;
                            }
                        }
                        key @ (KeyCode::Left | KeyCode::Right) => {
                            let step = if key == KeyCode::Left { -1 } else { 1 };
                            state.move_cursor(step);
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
                        let Some(table) = state
                            .output
                            .as_ref()
                            .and_then(|res| res.as_ref().ok()) else {
                                draw = false;
                                continue;
                            };

                        let entry = table.entries.get(table.focus as usize);

                        let preprint_id = entry.and_then(|entry| entry.eprint());

                        log::debug!("Preprint {:?}", preprint_id);

                        if let Some(preprint_id) = preprint_id {
                            state.fetching = true;
                            preprint_request.set(api::get_preprint(preprint_id.to_string()).fuse());
                        } else {
                            log::warn!("unable to find preprint link for {:?}", entry);
                            state.warning = Some(format!("Unable to find preprint link"));
                        }
                    }
                }
            }
            Message::SearchResponse(res) => {
                log::debug!("search response {:?}", res);

                state.output = Some(
                    res.map(|res| res.hits.hits.into_iter().map(|hit| hit.metadata).collect())
                        .map(|entries| TableState::new(entries)),
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
                    let path_id = match url
                        .rsplit_once("/")
                        .ok_or(anyhow::anyhow!("unexpected arxiv id {:?}", url))
                    {
                        Ok(path_id) => path_id.1,
                        Err(err) => {
                            state.fetching = false;
                            log::error!("{:?}", err);
                            state.warning = Some(format!("{}", err));
                            continue;
                        }
                    };
                    path.push(format!("{}.pdf", path_id));
                    path
                };

                if std::path::Path::new(&path).exists() {
                    state.fetching = false;
                    Command::new("xdg-open").arg(path).spawn();
                    continue;
                }

                let request = surf::Client::new()
                    .with(surf::middleware::Redirect::default())
                    .get(url.clone())
                    .map_err(surf::Error::into_inner);

                download_request.set(
                    request
                        .and_then(|mut res| {
                            res.take_body()
                                .into_bytes()
                                .map_err(surf::Error::into_inner)
                        })
                        .map(|res| res.context("error downloading preprint"))
                        .map_ok({
                            let path = path.clone();
                            move |bytes| {
                                std::fs::write(&path, bytes)
                                    .map_err(anyhow::Error::new)
                                    .context("error saving preprint")
                            }
                        })
                        .map_ok(|_| path)
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
            Message::Downloaded(result) => {
                state.fetching = false;

                let path = match result {
                    Ok(path) => path,
                    Err(err) => {
                        log::error!("{:?}", err);
                        state.warning = Some(format!("{}", err));
                        continue;
                    }
                };
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

struct TableState<T> {
    entries: Vec<T>,
    focus: u8,
    offset: u8,
}

impl<T> TableState<T> {
    fn new(entries: Vec<T>) -> Self {
        Self {
            entries,
            focus: 0,
            offset: 0,
        }
    }
    fn down(&mut self, step: u8) -> bool {
        if self.entries.is_empty() {
            return false;
        }

        let next_focus = (self.focus + step).min((self.entries.len() - 1) as u8);
        self.change_focus(next_focus)
    }

    fn up(&mut self, step: u8) -> bool {
        if self.entries.is_empty() {
            return false;
        }

        let next_focus = self.focus.saturating_sub(step);
        self.change_focus(next_focus)
    }

    /// (offset, marker)
    fn draw(&mut self, size: usize) -> (usize, usize) {
        let space = size as isize;
        let mut offset = self.offset as isize;
        let mut marker = self.focus as isize - offset;
        let entries = self.entries.len() as isize;

        log::trace!(
            "space/entries = {}/{} marker = {} offset = {}",
            space,
            entries,
            marker,
            offset
        );

        if (marker + 1) > space {
            offset += marker + 1 - space;
            marker = space - 1;
        } else if marker < 0 {
            offset += marker;
            marker = 0;
        }

        let showed = (entries - offset).min(space);

        if showed < entries && showed < space {
            let diff = space - showed;
            let next_offset = (offset - diff).min(0);
            marker += offset - next_offset;
            offset = next_offset;
        }

        log::trace!(
            "space/entries = {}/{} marker = {} offset = {}",
            space,
            entries,
            marker,
            offset
        );

        self.offset = offset as u8;

        (offset as usize, marker as usize)
    }

    fn change_focus(&mut self, focus: u8) -> bool {
        if focus == self.focus {
            return false;
        }

        self.focus = focus;
        true
    }
}

fn ui<'t, 's, B: tui::backend::Backend>(
    terminal: &'t mut Terminal<B>,
    state: &'s mut State,
    spinner: &'s SpinnerState,
) -> Result<tui::terminal::CompletedFrame<'t>, io::Error> {
    terminal.draw(|f| {
        let size = f.size();
        let block = Block::default().borders(Borders::ALL).title(Span::styled(
            "πνέω",
            Style::default().add_modifier(Modifier::BOLD),
        ));
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
        f.render_widget(
            Block::default().title(vec![Span::raw(&state.input)]),
            text_chunk,
        );
        f.set_cursor(text_chunk.x + state.cursor as u16, text_chunk.y);

        text_chunk.x -= 3;
        text_chunk.width = 4;
        f.render_widget(Block::default().title(spinner.icon()), text_chunk);

        let message_chunk = Layout::default()
            .margin(1)
            .constraints([Constraint::Length(3), Constraint::Min(0)].as_ref())
            .split(results_chunk)[0];

        let busy = state.busy();

        let table = match &mut state.output {
            None => None,
            Some(Ok(res)) => {
                if res.entries.is_empty() {
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

        if let Some(table) = table {
            let (offset, marker) = table.draw(results_chunk.height as usize);

            let rows = table
                .entries
                .iter()
                .skip(offset)
                .take(results_chunk.height as usize)
                .enumerate()
                .map(|(i, metadata)| {
                    let highlight = i == marker;
                    let mark = !busy && highlight;

                    Row::new(vec![
                        format!(
                            "{} {}",
                            if mark { ">" } else { " " },
                            metadata.title().unwrap_or("(No title)")
                        ),
                        metadata.authors(),
                    ])
                    .style(if highlight {
                        Style::default()
                            .bg(Color::LightBlue)
                            .add_modifier(Modifier::BOLD)
                            .fg(Color::Black)
                    } else {
                        Style::default()
                    })
                })
                .collect::<Vec<_>>();

            f.render_widget(
                Table::new(rows).widths(&[Constraint::Percentage(70), Constraint::Percentage(30)]),
                results_chunk,
            );

            if let Some(warning) = &state.warning {
                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints(
                        [
                            Constraint::Percentage(33),
                            Constraint::Percentage(33),
                            Constraint::Min(0),
                        ]
                        .as_ref(),
                    )
                    .split(size);

                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints(
                        [
                            Constraint::Percentage(30),
                            Constraint::Percentage(40),
                            Constraint::Min(0),
                        ]
                        .as_ref(),
                    )
                    .split(chunks[1]);

                let warning_block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(tui::widgets::BorderType::Double)
                    .border_style(Style::default().bg(Color::LightRed));

                f.render_widget(tui::widgets::Clear, chunks[1]);
                f.render_widget(warning_block, chunks[1]);

                let message_chunk = chunks[1].inner(&Margin {
                    // log::warn!("unable to find preprint link for {:?}", )
                    vertical: 3,
                    horizontal: 3,
                });
                f.render_widget(Block::default().title(&warning[..]), message_chunk);
            }
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
