use std::time::Duration;

use chrono::{Datelike, Local, NaiveDate, TimeDelta};
use data::client::{self, ClientsContext};
use data::config::actions::NicknameClickAction;
use data::dashboard::BufferAction;
use data::history::filter::FilterChain;
use data::history::{self, search, storage};
use data::target::{self, Target};
use data::user::Nick;
use data::{Config, Server, buffer, message, preview};
use iced::widget::{
    self, button, center, column, container, operation, pick_list, row, rule,
    scrollable, text, text_input,
};
use iced::{ContentFit, Length, Task, padding};
use tokio::time;

use super::{context_menu, message_feed, scroll_view};
use crate::widget::{Element, tooltip};
use crate::{Theme, font, icon, theme};

const DEBOUNCE: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
pub enum Message {
    Input(String),
    SetFilter(&'static str, Option<String>),
    SetPeriod(Option<String>),
    ClearFilters,
    Debounced(u64),
    Results(u64, search::Range, Result<search::Page, search::Error>),
    LoadMore,
    Scrolled(bool),
    Feed(scroll_view::Message),
}

pub enum Event {
    ContextMenu(context_menu::Event),
    OpenBuffer(Server, Target, BufferAction),
    GoToMessage(buffer::Upstream, message::MessageLink, BufferAction),
    OpenUrl(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Idle,
    Stale,
    Loading,
    Loaded,
    Updating,
    Failed(String),
}

#[derive(Debug, Clone)]
struct Hit {
    buffer: buffer::Upstream,
    kind: history::Kind,
    date: NaiveDate,
    message: data::Message,
}

#[derive(Debug, Clone)]
pub struct Search {
    pub query: String,
    parsed: search::Query,
    input_id: widget::Id,
    scrollable_id: widget::Id,
    generation: u64,
    status: Status,
    hits: Vec<Hit>,
    newest: Option<search::Cursor>,
    next: Option<search::Cursor>,
    /// The last page stopped scanning before filling up.
    partial: bool,
    filters: search::Filters,
    behind: bool,
    scrolled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServerFilter {
    All,
    Server { value: String, label: String },
}

impl std::fmt::Display for ServerFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerFilter::All => write!(f, "All servers"),
            ServerFilter::Server { label, .. } => write!(f, "{label}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KindFilter {
    All,
    Channels,
    Queries,
}

impl KindFilter {
    const ALL: [Self; 3] = [Self::All, Self::Channels, Self::Queries];

    fn value(self) -> Option<&'static str> {
        match self {
            KindFilter::All => None,
            KindFilter::Channels => Some("channel"),
            KindFilter::Queries => Some("query"),
        }
    }
}

impl std::fmt::Display for KindFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KindFilter::All => write!(f, "Channels and queries"),
            KindFilter::Channels => write!(f, "Channels"),
            KindFilter::Queries => write!(f, "Queries"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Period {
    Any,
    Today,
    Week,
    Month,
    Year,
}

impl Period {
    const ALL: [Self; 5] =
        [Self::Any, Self::Today, Self::Week, Self::Month, Self::Year];

    fn days(self) -> Option<i64> {
        match self {
            Period::Any => None,
            // `after:` includes its day, so today counts as one.
            Period::Today => Some(0),
            Period::Week => Some(6),
            Period::Month => Some(29),
            Period::Year => Some(364),
        }
    }

    fn after(self, today: NaiveDate) -> Option<String> {
        let date = today - TimeDelta::days(self.days()?);
        Some(date.format("%Y-%m-%d").to_string())
    }

    /// The period `filters` limit to, unless they use other dates.
    fn of(filters: &search::Filters, today: NaiveDate) -> Option<Self> {
        if filters.value("before").is_some() || filters.value("on").is_some() {
            return None;
        }
        // The latest `after:` is the one that applies.
        let Some(after) = filters.values("after").max_by_key(|value| {
            NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()
        }) else {
            return Some(Period::Any);
        };
        Self::ALL
            .into_iter()
            .find(|period| period.after(today).as_deref() == Some(after))
    }
}

impl std::fmt::Display for Period {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Period::Any => write!(f, "Any date"),
            Period::Today => write!(f, "Today"),
            Period::Week => write!(f, "Past 7 days"),
            Period::Month => write!(f, "Past 30 days"),
            Period::Year => write!(f, "Past year"),
        }
    }
}

impl Search {
    pub fn new(query: Option<String>) -> Self {
        let mut search = Self {
            query: String::new(),
            parsed: search::Query::default(),
            input_id: widget::Id::unique(),
            scrollable_id: widget::Id::unique(),
            generation: 0,
            status: Status::Idle,
            hits: vec![],
            newest: None,
            next: None,
            partial: false,
            filters: search::Filters::default(),
            behind: false,
            scrolled: false,
        };
        search.set_query(query.unwrap_or_default());
        search
    }

    pub fn set_query(&mut self, query: String) {
        self.hits.clear();
        self.change_query(query);
    }

    fn change_query(&mut self, query: String) {
        self.query = query;
        self.generation += 1;
        self.newest = None;
        self.next = None;
        self.partial = false;
        self.behind = false;
        self.parsed = search::Query::parse(&self.query);
        self.filters = search::Filters::of(&self.query);
        self.status = if self.parsed.is_runnable() {
            Status::Stale
        } else {
            self.hits.clear();
            Status::Idle
        };
    }

    pub fn has_more(&self) -> bool {
        self.next.is_some()
    }

    pub fn result_count(&self) -> Option<usize> {
        self.is_loaded().then_some(self.hits.len())
    }

    fn is_loaded(&self) -> bool {
        matches!(self.status, Status::Loaded | Status::Updating)
    }

    pub fn indexed(&mut self, storage: &storage::Manager) -> Task<Message> {
        self.behind = true;
        self.catch_up(storage)
    }

    pub fn update(
        &mut self,
        message: Message,
        clients: &client::Map,
        storage: &storage::Manager,
        config: &Config,
    ) -> (Task<Message>, Option<Event>) {
        match message {
            Message::Input(query) => {
                self.change_query(query);

                if self.status == Status::Stale {
                    let generation = self.generation;

                    (
                        Task::perform(time::sleep(DEBOUNCE), move |()| {
                            Message::Debounced(generation)
                        }),
                        None,
                    )
                } else {
                    (Task::none(), None)
                }
            }
            Message::SetFilter(key, value) => {
                let query =
                    search::set_filter(&self.query, key, value.as_deref());
                self.update(Message::Input(query), clients, storage, config)
            }
            Message::SetPeriod(after) => {
                let query = ["on", "before"]
                    .into_iter()
                    .fold(self.query.clone(), |query, key| {
                        search::set_filter(&query, key, None)
                    });
                let query =
                    search::set_filter(&query, "after", after.as_deref());
                self.update(Message::Input(query), clients, storage, config)
            }
            Message::ClearFilters => {
                let query = search::clear_filters(&self.query);
                self.update(Message::Input(query), clients, storage, config)
            }
            Message::Debounced(generation) => {
                if generation == self.generation && self.status == Status::Stale
                {
                    (self.run(storage, search::Range::Latest), None)
                } else {
                    (Task::none(), None)
                }
            }
            Message::LoadMore => {
                if self.status == Status::Loaded
                    && let Some(next) = self.next
                {
                    (self.run(storage, search::Range::Before(next)), None)
                } else {
                    (Task::none(), None)
                }
            }
            Message::Scrolled(scrolled) => {
                self.scrolled = scrolled;
                (self.catch_up(storage), None)
            }
            Message::Results(generation, range, result) => {
                if generation != self.generation {
                    return (Task::none(), None);
                }

                let page = match result {
                    Ok(page) => page,
                    Err(error) => {
                        log::warn!("search failed: {error}");
                        if range == search::Range::Latest {
                            self.hits.clear();
                            self.next = None;
                        }
                        self.status = Status::Failed(error.to_string());
                        return (Task::none(), None);
                    }
                };

                let servers = clients
                    .servers()
                    .map(|server| (format!("{server:b}"), server))
                    .collect::<Vec<_>>();
                let partial =
                    page.next.is_some() && page.hits.len() < search::PAGE_SIZE;
                let hits = page
                    .hits
                    .into_iter()
                    .filter_map(|hit| resolve(hit, &servers, clients));

                let task = match range {
                    search::Range::Before(_) => {
                        self.hits.extend(hits);
                        self.next = page.next;
                        self.partial = partial;
                        Task::none()
                    }
                    // Newer pages start at the current hits, so there's no gap.
                    search::Range::After(_) => {
                        self.hits.splice(0..0, hits);
                        self.newest = page.newest.or(self.newest);
                        // Keep going until the newest hit is caught up.
                        self.behind |= page.next.is_some();
                        Task::none()
                    }
                    search::Range::Latest => {
                        self.hits.clear();
                        self.hits.extend(hits);
                        self.newest = page.newest;
                        self.next = page.next;
                        self.partial = partial;
                        self.scrolled = false;
                        operation::snap_to(
                            self.scrollable_id.clone(),
                            scrollable::RelativeOffset::START,
                            operation::Animation::Instant,
                        )
                    }
                };
                self.status = Status::Loaded;

                (Task::batch([task, self.catch_up(storage)]), None)
            }
            Message::Feed(message) => {
                (Task::none(), feed_event(message, config))
            }
        }
    }

    fn catch_up(&mut self, storage: &storage::Manager) -> Task<Message> {
        if self.behind && !self.scrolled && self.status == Status::Loaded {
            self.behind = false;
            let range = self
                .newest
                .map_or(search::Range::Latest, search::Range::After);
            self.run(storage, range)
        } else {
            Task::none()
        }
    }

    fn run(
        &mut self,
        storage: &storage::Manager,
        range: search::Range,
    ) -> Task<Message> {
        self.status = if self.status == Status::Loaded {
            Status::Updating
        } else {
            Status::Loading
        };

        let generation = self.generation;

        Task::perform(
            storage.search(self.parsed.clone(), range),
            move |result| Message::Results(generation, range, result),
        )
    }

    pub fn focus(&self) -> Task<Message> {
        let input_id = self.input_id.clone();

        let focus =
            operation::is_focused(input_id.clone()).then(move |is_focused| {
                if is_focused {
                    Task::none()
                } else {
                    operation::focus(input_id.clone())
                }
            });

        if self.status == Status::Stale {
            focus.chain(Task::done(Message::Debounced(self.generation)))
        } else {
            focus
        }
    }
}

fn resolve(
    hit: search::Hit,
    servers: &[(String, &Server)],
    clients: &client::Map,
) -> Option<Hit> {
    let server = servers
        .iter()
        .find_map(|(key, server)| (*key == hit.server).then_some(*server))?
        .clone();

    let buffer = match hit.buffer {
        search::Buffer::Server => buffer::Upstream::Server(server),
        search::Buffer::Channel(channel) => {
            let channel = target::Channel::from_str(
                &channel,
                clients.get_server_chantypes_or_default(&server),
                clients.get_server_casemapping_or_default(&server),
            );
            buffer::Upstream::Channel(server, channel)
        }
        search::Buffer::Query(query) => {
            let query = target::Query::from(Nick::from_str(
                &query,
                clients.get_server_casemapping_or_default(&server),
            ));
            buffer::Upstream::Query(server, query)
        }
    };

    Some(Hit {
        kind: history::Kind::from(buffer.clone()),
        date: hit.message.time.utc.with_timezone(&Local).date_naive(),
        buffer,
        message: hit.message,
    })
}

fn feed_event(message: scroll_view::Message, config: &Config) -> Option<Event> {
    match message {
        scroll_view::Message::ContextMenu(message) => {
            context_menu::update(message).map(Event::ContextMenu)
        }
        scroll_view::Message::Link(link) => match link {
            message::Link::GoToMessage(buffer, message, buffer_action) => {
                Some(Event::GoToMessage(
                    buffer,
                    message,
                    buffer_action.unwrap_or_default(),
                ))
            }
            message::Link::Channel(server, channel, buffer_action) => {
                buffer_action.map(|buffer_action| {
                    Event::OpenBuffer(
                        server,
                        Target::Channel(channel),
                        buffer_action,
                    )
                })
            }
            message::Link::Url(url) => Some(Event::OpenUrl(url)),
            message::Link::User(server, user) => {
                match config.actions.buffer.click_username {
                    NicknameClickAction::OpenQuery(buffer_action) => {
                        Some(Event::OpenBuffer(
                            server,
                            Target::Query(target::Query::from(user)),
                            buffer_action,
                        ))
                    }
                    NicknameClickAction::InsertNickname
                    | NicknameClickAction::Noop => None,
                }
            }
            message::Link::ExpandMessage(..)
            | message::Link::ContractMessage(..) => None,
        },
        _ => None,
    }
}

pub fn view<'a>(
    state: &'a Search,
    clients: &'a client::Map,
    previews: &'a preview::Collection,
    filter_chain: FilterChain<'_>,
    config: &'a Config,
    theme: &'a Theme,
    channels_context: &'a dyn context_menu::ChannelsContext,
) -> Element<'a, Message> {
    let header = container(
        column![
            row![
                text_input("Search messages...", &state.query)
                    .id(state.input_id.clone())
                    .style(move |theme, status| {
                        if matches!(status, text_input::Status::Disabled) {
                            theme::text_input::primary(
                                theme,
                                text_input::Status::Active,
                            )
                        } else {
                            theme::text_input::primary(theme, status)
                        }
                    })
                    .on_input(Message::Input),
            ]
            .spacing(8)
            .padding(padding::top(8)),
            filter_bar(state, clients, theme),
            container(rule::horizontal(1)).width(Length::Fill)
        ]
        .spacing(8),
    )
    .padding(padding::horizontal(4))
    .width(Length::Fill);

    let show_dates = config.buffer.date_separators.show;
    let show_year = !has_year(&config.buffer.date_separators.format);
    let this_year = Local::now().year();
    let mut last_date = None;
    let mut rows = vec![];

    for hit in state.hits.iter().filter(|hit| {
        !filter_chain.filter_message_of_kind(&hit.message, &hit.kind)
    }) {
        let Some(row) = message_feed::message_row(
            &hit.message,
            hit.buffer.clone(),
            hit.buffer.as_server(),
            hit.buffer.as_channel(),
            message::MessageLink::Message(
                hit.message.history_id,
                hit.message.time,
            ),
            clients,
            previews,
            config,
            theme,
            channels_context,
        ) else {
            continue;
        };

        if show_dates && last_date != Some(hit.date) {
            last_date = Some(hit.date);

            let mut label = config.buffer.format_date_separator(&hit.date);
            if show_year && hit.date.year() != this_year {
                label = format!("{label}, {}", hit.date.year());
            }

            rows.push(scroll_view::date_separator(label, config, theme));
        }

        rows.push(row.map(Message::Feed));
    }

    let body: Element<'a, Message> = if rows.is_empty() {
        let reason = match &state.status {
            Status::Idle if !state.parsed.errors().is_empty() => {
                Some(Reason::Error(state.parsed.errors().join("\n")))
            }
            Status::Idle => None,
            Status::Stale | Status::Loading => Some(Reason::Status("...")),
            Status::Loaded | Status::Updating if state.has_more() => {
                Some(Reason::Status("No messages found in the newest messages"))
            }
            Status::Loaded | Status::Updating => {
                Some(Reason::Status("No messages found"))
            }
            Status::Failed(error) => Some(Reason::Error(error.clone())),
        };

        placeholder(reason, load_more(state, theme), theme)
    } else {
        scrollable(
            column(rows)
                .push(load_more(state, theme))
                .spacing(config.buffer.line_spacing)
                .padding(8),
        )
        .direction(scrollable::Direction::Vertical(
            scrollable::Scrollbar::default()
                .width(config.pane.scrollbar.width)
                .scroller_width(config.pane.scrollbar.scroller_width),
        ))
        .on_scroll(|scroll| {
            Message::Scrolled(scroll.viewport.absolute_offset().y > 0.0)
        })
        .id(state.scrollable_id.clone())
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    };

    let content = column![header, body].spacing(1).padding([2, 2]);

    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn has_year(format: &str) -> bool {
    // Same month, day and weekday; only the year differs.
    let date = |year| {
        NaiveDate::from_ymd_opt(year, 1, 1)
            .map(|date| date.format(format).to_string())
    };
    date(2001) != date(2007)
}

fn filter_bar<'a>(
    state: &'a Search,
    clients: &'a client::Map,
    theme: &'a Theme,
) -> Element<'a, Message> {
    let filters = &state.filters;
    let today = Local::now().date_naive();

    let mut servers = vec![ServerFilter::All];
    for server in clients.servers() {
        let server = ServerFilter::Server {
            value: search::server_value(server, clients.servers()),
            label: if server.is_bouncer_network() {
                format!("{server} ({})", server.name)
            } else {
                server.to_string()
            },
        };
        if !servers.contains(&server) {
            servers.push(server);
        }
    }
    // Several servers, or names the list doesn't have, show as custom.
    let server = match filters.values("server").collect::<Vec<_>>()[..] {
        [] => Some(ServerFilter::All),
        [selected] => servers
            .iter()
            .find(|server| {
                matches!(
                    server,
                    ServerFilter::Server { value, .. }
                        if value.eq_ignore_ascii_case(selected)
                )
            })
            .cloned(),
        _ => None,
    };

    let kind = match filters.value("is") {
        None => Some(KindFilter::All),
        Some(value) => KindFilter::ALL.into_iter().find(|kind| {
            kind.value()
                .is_some_and(|kind| value.eq_ignore_ascii_case(kind))
        }),
    };

    let case_sensitive = filters
        .value("case")
        .is_some_and(|value| value.eq_ignore_ascii_case("sensitive"));

    let control_padding = [4, 8];

    row![
        pick_list(server, servers, ServerFilter::to_string)
            .placeholder("Custom servers")
            .on_select(|server| {
                Message::SetFilter(
                    "server",
                    match server {
                        ServerFilter::All => None,
                        ServerFilter::Server { value, .. } => Some(value),
                    },
                )
            })
            .padding(control_padding),
        pick_list(kind, KindFilter::ALL, KindFilter::to_string)
            .placeholder("Custom buffers")
            .on_select(|kind| {
                Message::SetFilter("is", kind.value().map(str::to_owned))
            })
            .padding(control_padding),
        pick_list(Period::of(filters, today), Period::ALL, Period::to_string)
            .placeholder("Custom dates")
            .on_select(move |period| Message::SetPeriod(period.after(today)))
            .padding(control_padding),
        tooltip(
            button(text("Aa"))
                .style(move |theme, status| {
                    theme::button::secondary(theme, status, case_sensitive)
                })
                .padding(control_padding)
                .on_press(Message::SetFilter(
                    "case",
                    (!case_sensitive).then(|| "sensitive".to_owned()),
                )),
            Some("Match case"),
            tooltip::Position::Bottom,
            theme,
        ),
        button(text("Clear filters"))
            .style(|theme, status| {
                theme::button::secondary(theme, status, false)
            })
            .padding(control_padding)
            .on_press_maybe(filters.any().then_some(Message::ClearFilters),),
    ]
    .spacing(4)
    .wrap()
    .vertical_spacing(4)
    .into()
}

fn load_more<'a>(
    state: &Search,
    theme: &'a Theme,
) -> Option<Element<'a, Message>> {
    (state.has_more() && state.is_loaded()).then(|| {
        // Regex and case filters stop after scanning part of the history.
        let note = state.partial.then(|| {
            text(
                "Searched part of the history. Load more to keep searching older messages.",
            )
            .style(theme::text::secondary)
            .font_maybe(theme::font_style::secondary(theme).map(font::get))
        });

        container(
            column![
                note,
                button(text("Load more"))
                    .style(|theme, status| {
                        theme::button::secondary(theme, status, false)
                    })
                    .on_press(Message::LoadMore),
            ]
            .spacing(8)
            .align_x(iced::Alignment::Center),
        )
        .center_x(Length::Fill)
        .padding(8)
        .into()
    })
}

enum Reason {
    Status(&'static str),
    Error(String),
}

fn placeholder<'a>(
    reason: Option<Reason>,
    footer: Option<Element<'a, Message>>,
    theme: &'a Theme,
) -> Element<'a, Message> {
    let content = match reason {
        Some(Reason::Status(status)) => column![
            text(status)
                .style(theme::text::secondary)
                .font_maybe(theme::font_style::secondary(theme).map(font::get)),
            footer,
        ]
        .align_x(iced::Alignment::Center),
        Some(Reason::Error(error)) => column![
            text(error)
                .style(theme::text::error)
                .font_maybe(theme::font_style::error(theme).map(font::get)),
            footer,
        ]
        .align_x(iced::Alignment::Center),
        None => column![
            icon::search()
                .width(Length::Shrink)
                .content_fit(ContentFit::Contain)
                .height(theme::TEXT_SIZE + 3.0)
                .style(theme::text::secondary),
            text("Search messages")
                .style(theme::text::secondary)
                .font_maybe(theme::font_style::secondary(theme).map(font::get)),
        ]
        .spacing(8)
        .align_x(iced::Alignment::Center),
    };

    center(content).into()
}
