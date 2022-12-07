use anyhow::{Context, Result};
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::{
    future::{Fuse, FusedFuture},
    stream::{self, FusedStream},
    Future, FutureExt, StreamExt, TryFutureExt,
};
use rusqlite::Connection;
use std::{
    collections::HashMap,
    io::{self, Write},
    panic::AssertUnwindSafe,
    path::Path,
    pin::Pin,
    process::Command,
    sync::Arc,
    task::Poll,
    time::{Duration, SystemTime},
};
use tui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Span,
    widgets::{Block, Borders, Paragraph, Row, Table},
    Terminal,
};

use crate::{
    api::{InspiresSearchResult, Metadata, NestedHit},
    cache::Cache,
    store::{Record, StoreConnection},
};

mod api;
mod cache;
mod store;

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
        let mut dir = data_dir.clone();
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
        let mut builder = env_logger::Builder::from_env(
            env_logger::Env::default().filter_or("PNEO_LOG", "pneo=info"),
        );
        let logfile = {
            let mut logfile = runtime_dir;
            logfile.push("pneo.log");
            logfile
        };

        builder.target(env_logger::Target::Pipe(Box::new(
            std::fs::File::create(logfile).context("Unable to create logfile")?,
        )));

        builder.init();
    }

    if std::env::args_os().skip(1).next() == Some("--version".into()) {
        println!("pneo {}", std::env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let (cache_connection, store_connection) = {
        let path = {
            let mut db = data_dir;
            db.push("pneo.db");
            db
        };
        let get = || Connection::open(&path).context("unable to create database");

        (get()?, get()?)
    };

    let mut cache = Cache::new(cache_connection, preprint_dir);
    let store = StoreConnection::start(store_connection);

    store.execute(|store| store.init());

    cache.init().context("unable to initialize database")?;

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

    let assert = AssertUnwindSafe((&mut terminal, cache, store));
    let result = std::panic::catch_unwind(|| {
        let assert = assert;
        main_loop(assert.0 .0, assert.0 .1, assert.0 .2)
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
    downloaded: HashMap<String, u8>,
}

struct Hitbox {
    table_size: Rect,
    /// (offset, height)
    table: (usize, usize),
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

    fn down(&mut self, step: u16) -> bool {
        if self.busy() {
            return false;
        }

        let Some(Ok(table)) = &mut self.output else {
            return false;
        };

        table.down(step)
    }

    fn up(&mut self, step: u16) -> bool {
        if self.busy() {
            return false;
        }

        let Some(Ok(table)) = &mut self.output else {
            return false;
        };

        table.up(step)
    }

    fn click_entry(&mut self, row: u16) -> bool {
        if self.busy() {
            return false;
        }

        let Some(Ok(table)) = &mut self.output else {
            return false;
        };

        table.set(row)
    }
}

fn main_loop<B: tui::backend::Backend>(
    terminal: &mut Terminal<B>,
    cache: Cache,
    store: StoreConnection,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .context("unable to start runtime")?;

    let _guard = runtime.enter();

    let cache = Arc::new(cache);

    let mut state = State {
        input: String::new(),
        cursor: 0,
        output: None,
        searching: false,
        fetching: false,
        warning: None,
        downloaded: cache.get_downloaded()?,
    };

    let mut hitbox = Hitbox {
        table_size: Default::default(),
        table: (0, 0),
    };

    let mut spinner_state = SpinnerState::new();

    let mut draw = true;
    let mut redraw = false;
    let mut last_click = (std::time::UNIX_EPOCH, u16::MAX);

    let search_request = Fuse::terminated();
    let preprint_request = Fuse::terminated();
    let mut event_stream = EventStream::new().fuse();
    let throttle = Fuse::terminated();
    futures::pin_mut!(search_request);
    futures::pin_mut!(preprint_request);
    futures::pin_mut!(throttle);

    loop {
        spinner_state.spin(state.busy());

        if draw {
            log::debug!("drawing!");
            ui(terminal, &mut state, &spinner_state, &mut hitbox, redraw)?;
        }

        draw = true;
        redraw = false;

        enum Message {
            Event(Option<io::Result<Event>>),
            SearchResponse(surf::Result<InspiresSearchResult>),
            Preprint(Result<()>),
            Spin,
            Commit,
        }

        #[derive(Debug)]
        enum MessageDebug<'a> {
            Event(Option<&'a Event>),
            SearchResponse(&'a surf::Result<InspiresSearchResult>),
            Preprint(&'a Result<()>),
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
                    Message::Preprint(res) => MessageDebug::Preprint(res),
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
                preprint = &mut preprint_request => Message::Preprint(preprint),
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
                        MouseEventKind::Up(MouseButton::Left) => {
                            let MouseEvent { column, row, .. } = mouse;

                            let click = Rect {
                                x: column,
                                y: row,
                                width: 1,
                                height: 1,
                            };

                            if !hitbox.table_size.intersects(click) {
                                continue;
                            }

                            let index = click.y - hitbox.table_size.y + (hitbox.table.0 as u16);

                            let changed = state.click_entry(index);

                            let clicked = state
                                .output
                                .as_ref()
                                .map(|res| res.as_ref().ok())
                                .flatten()
                                .map(|table| table.focus == index)
                                .unwrap_or(false);

                            let in_window = last_click
                                .0
                                .elapsed()
                                .ok()
                                .map(|dur| dur < Duration::from_millis(500))
                                .unwrap_or(true);

                            let same_row = last_click.1 == index;

                            if clicked {
                                last_click = (SystemTime::now(), index);
                            }

                            if in_window && same_row {
                                Some(Action::Select)
                            } else if changed {
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
                            if key.modifiers == KeyModifiers::CONTROL && ch == 'r' {
                                draw = true;
                                redraw = true;
                                continue;
                            }
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

                        let Some(entry) = entry else {
                            log::warn!("unable to find preprint link for {:?}", entry);
                            state.warning = Some(format!("Unable to find preprint link"));
                            continue;
                        };

                        let preprint_id = {
                            let mut eprints = entry.eprints();
                            if eprints.len() > 1 {
                                log::warn!("multiple eprints for {:?}", entry);
                            }
                            eprints.next()
                        };

                        log::debug!("preprint {:?}", preprint_id);

                        match preprint_id
                            .as_ref()
                            .map(|id| cache.preprint_file_from_id(id))
                        {
                            None => {
                                log::warn!("unable to find preprint link for {:?}", entry);
                                state.warning = Some(format!("Unable to find preprint link"));
                            }
                            Some(Err(err)) => {
                                state.warning = Some(format!("{}", err));
                            }
                            Some(Ok(None)) => {
                                state.fetching = true;
                                preprint_request.set(
                                    download(preprint_id.unwrap().to_owned(), cache.clone()).fuse(),
                                );
                            }
                            Some(Ok(Some(filename))) => {
                                state.warning = open_preprint(Path::new(&filename))
                                    .err()
                                    .map(|err| err.to_string());
                            }
                        }
                    }
                }
            }
            Message::SearchResponse(res) => {
                log::debug!("search response {:?}", res);

                let hits = res.map(|res| res.hits.hits);

                let records = hits
                    .as_ref()
                    .into_iter()
                    .flat_map(|entries: &Vec<NestedHit>| entries.iter())
                    .filter_map(|hit| {
                        Some(Record {
                            control_number: hit.metadata.control_number,
                            title: hit.metadata.title().unwrap_or("").to_string(),
                            authors: hit
                                .metadata
                                .authors
                                .iter()
                                .filter_map(|au| au.last_name.clone())
                                .collect(),
                            created: hit.created_date()?.to_string(),
                        })
                    })
                    .collect();

                store.execute(|mut store| store.upsert(records));

                state.output = Some(hits.map(|hits| {
                    TableState::new(hits.into_iter().map(|hit| hit.metadata).collect())
                }));

                state.searching = false;
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
            Message::Preprint(res) => {
                state.fetching = false;
                let update = cache.get_downloaded().map_err(|e| e.to_string());

                if let Err(err) = res {
                    log::error!("{:?}", err);
                    state.warning = Some(format!("{}", err));
                    continue;
                }

                match update {
                    Ok(downloaded) => state.downloaded = downloaded,
                    Err(error) => {
                        state.warning = Some(error);
                    }
                }
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
    focus: u16,
    offset: u16,
}

impl<T> TableState<T> {
    fn new(entries: Vec<T>) -> Self {
        Self {
            entries,
            focus: 0,
            offset: 0,
        }
    }
    fn down(&mut self, step: u16) -> bool {
        if self.entries.is_empty() {
            return false;
        }

        let next_focus = (self.focus + step).min((self.entries.len() - 1) as u16);
        self.change_focus(next_focus)
    }

    fn up(&mut self, step: u16) -> bool {
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

        self.offset = offset as u16;

        (offset as usize, marker as usize)
    }

    fn change_focus(&mut self, focus: u16) -> bool {
        if focus == self.focus {
            return false;
        }

        self.focus = focus;
        true
    }

    fn set(&mut self, row: u16) -> bool {
        if self.entries.is_empty() {
            return false;
        }

        self.change_focus(row)
    }
}

fn ui<'t, 's, B: tui::backend::Backend>(
    terminal: &'t mut Terminal<B>,
    state: &'s mut State,
    spinner: &'s SpinnerState,
    hitbox: &'s mut Hitbox,
    redraw: bool,
) -> Result<tui::terminal::CompletedFrame<'t>, io::Error> {
    if redraw {
        terminal.clear()?;
    }
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
                    let version = metadata
                        .eprint()
                        .and_then(|id| state.downloaded.get(id).copied());
                    let downloaded = version.is_some();

                    Row::new(vec![
                        format!(
                            "{} {} {}",
                            if mark { ">" } else { " " },
                            if downloaded { "✔" } else { " " },
                            metadata.title().unwrap_or("(No title)")
                        ),
                        metadata
                            .eprint()
                            .map(|eprint| {
                                if let Some(version) = version {
                                    format!("{}v{}", eprint, version)
                                } else {
                                    eprint.to_string()
                                }
                            })
                            .unwrap_or_default(),
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

            hitbox.table = (
                offset,
                (results_chunk.height as usize).min(table.entries.len() - offset),
            );

            hitbox.table_size = results_chunk;

            f.render_widget(
                Table::new(rows).widths(&[
                    Constraint::Percentage(65),
                    Constraint::Percentage(10),
                    Constraint::Percentage(25),
                ]),
                results_chunk,
            );

            if let Some(warning) = &state.warning {
                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints(
                        [
                            Constraint::Percentage(25),
                            Constraint::Percentage(50),
                            Constraint::Min(0),
                        ]
                        .as_ref(),
                    )
                    .split(size);

                let padding = 1;

                let warning_rect = {
                    let width = chunks[1].width;
                    let margin = 1 + padding;
                    let effective_width = width - 2 * margin;
                    let effective_lines = warning.lines().fold(0, |acc, line| {
                        acc + (line.len() as u16 / effective_width) + 1
                    });

                    let mut rect = chunks[1];
                    let height = rect.height;

                    let desired_height = effective_lines + 2 * margin;

                    rect.height = desired_height.max(rect.height / 3);
                    rect.y = (height - rect.height) / 2;
                    rect
                };

                f.render_widget(tui::widgets::Clear, warning_rect);

                let outer_block = Block::default()
                    .borders(Borders::ALL)
                    .border_type(tui::widgets::BorderType::Double)
                    .border_style(Style::default().bg(Color::LightRed));

                let inner = {
                    let mut inner = outer_block.inner(warning_rect);

                    inner.x += padding;
                    inner.y += padding;
                    inner.height -= padding;
                    inner.width -= padding;
                    inner
                };

                f.render_widget(outer_block, warning_rect);

                f.render_widget(
                    Paragraph::new(&warning[..]).wrap(tui::widgets::Wrap { trim: false }),
                    inner,
                );
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

async fn download(preprint_id: String, cache: Arc<Cache>) -> Result<()> {
    log::info!("requesting preprint {}", &preprint_id);

    let preprint = api::get_preprint(preprint_id.to_string())
        .await
        .map_err(surf::Error::into_inner)
        .context("error downloading preprint info")?;

    let entry = if preprint.entry.len() == 1 {
        &preprint.entry[0]
    } else {
        anyhow::bail!("invalid preprint response {:?}", preprint)
    };

    let link = entry
        .link
        .iter()
        .filter(|link| link.title.as_deref() == Some("pdf"))
        .next();

    let Some(link) = link else {
        anyhow::bail!("unable to find pdf link");
    };

    let url = &link.href;

    log::info!("downloading {}", url);

    let mut response = surf::Client::new()
        .with(surf::middleware::Redirect::default())
        .get(&url)
        .map_err(surf::Error::into_inner)
        .await
        .context("error downloading pdf")?;

    let bytes = response
        .body_bytes()
        .await
        .map_err(surf::Error::into_inner)?;

    let path = cache
        .insert(&entry.id, &preprint_id, &url, bytes)
        .context("error saving preprint")?;

    open_preprint(&path)
}

fn open_preprint(path: &Path) -> Result<()> {
    log::info!("opening {:?}", path);

    let mut child = Command::new("xdg-open")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("unable to open preprint")?;

    let status = child.wait()?;

    if status.success() {
        return Ok(());
    }

    let output = child.wait_with_output()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let error = anyhow::anyhow!(
        "unable to open preprint, xdg-open failed with {}\n{}\n{}",
        status,
        stdout,
        stderr
    );

    log::error!("{:?}", error);

    Err(error)
}
