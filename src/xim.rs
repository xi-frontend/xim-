use std::io::{self, Write};
use std::collections::HashMap;

use futures::{future, Async, Future, Poll, Sink, Stream};
use futures::sync::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};

use termion::event::{Event, Key};
use tokio_core::reactor::Handle;
use xrl::{
    Client, ClientResult, Frontend,
    FrontendBuilder, ScrollTo, ServerResult,
    Style, Update, AvailablePlugins,
    PluginStarted, PluginStoped, UpdateCmds,
    ConfigChanged, ThemeChanged, ViewId
};

use xdg::BaseDirectories;

use errors::*;
use terminal::{Terminal, TerminalEvent};
use view::{View, ViewClient};

pub struct Xim {
    pub pending_open_requests: Vec<ClientResult<(ViewId, View)>>,
    pub delayed_events: Vec<CoreEvent>,
    pub views: HashMap<ViewId, View>,
    pub current_view: Option<ViewId>,
    pub events: UnboundedReceiver<CoreEvent>,
    pub handle: Handle,
    pub client: Client,
    pub term: Terminal,
    pub term_size: (u16, u16),
    pub shutdown: bool,
    pub styles: HashMap<u64, Style>,
}
	
impl Xim {
    pub fn new(
        handle: Handle,
        mut client: Client,
        events: UnboundedReceiver<CoreEvent>,
    ) -> Result<Self> {
        let mut styles = HashMap::new();
        styles.insert(0, Default::default());
        if let Ok(dirs) = BaseDirectories::with_prefix("xi") {
            if let Some(conf_dir) = dirs.get_config_home().to_str() {
                handle.spawn(client.client_started(Some(conf_dir)).map_err(|_|()));
            }
        }
        Ok(Xim {
            events: events,
            delayed_events: Vec::new(),
            pending_open_requests: Vec::new(),
            handle: handle,
            term: Terminal::new()?,
            term_size: (0, 0),
            views: HashMap::new(),
            styles: styles,
            current_view: None,
            client: client,
            shutdown: false,
        })
    }

    fn dispatch_core_event(&mut self, event: CoreEvent) {
        match event {
            CoreEvent::Update(update) => self.handle_update(update),
            CoreEvent::SetStyle(style) => self.handle_def_style(style),
            CoreEvent::ScrollTo(scroll_to) => self.handle_scroll_to(scroll_to),
        }
    }

    fn handle_update(&mut self, update: Update) {
        let Xim {
            ref mut views,
            ref mut delayed_events,
            ..
        } = *self;
        match views.get_mut(&update.view_id) {
            Some(view) => view.update_cache(update),
            None => delayed_events.push(CoreEvent::Update(update)),
        }
    }

    fn handle_scroll_to(&mut self, scroll_to: ScrollTo) {
        let Xim {
            ref mut views,
            ref mut delayed_events,
            ..
        } = *self;
        match views.get_mut(&scroll_to.view_id) {
            Some(view) => view.set_cursor(scroll_to.line, scroll_to.column),
            None => delayed_events.push(CoreEvent::ScrollTo(scroll_to)),
        }
    }

    fn handle_def_style(&mut self, style: Style) {
        self.styles.insert(style.id, style);
    }

    fn handle_resize(&mut self, size: (u16, u16)) {
        let Xim {
            ref mut views,
            ref current_view,
            ..
        } = *self;
        info!("setting new terminal size");
        self.term_size = size;
        if let Some(current_view) = *current_view {
            if let Some(view) = views.get_mut(&current_view) {
                view.resize(size.1);
            } else {
                warn!("view {:?} not found", current_view);
            }
        }
    }

    pub fn open(&mut self, file_path: String) {
        let client = self.client.clone();
        let handle = self.handle.clone();
        let task = self.client
            .new_view(Some(file_path.clone()))
            .and_then(move |view_id| {
                let view_client = ViewClient::new(client, handle, view_id.clone());
                Ok((view_id, View::new(view_client, Some(file_path))))
            });
        self.pending_open_requests.push(Box::new(task));
    }

    fn exit(&mut self) {
        self.shutdown = true;
    }

    fn handle_input(&mut self, event: Event) {
        match event {
            Event::Key(Key::Ctrl('q')) => self.exit(),
            Event::Key(Key::F(4)) => self.set_theme("base16-eighties.dark"),
            Event::Key(Key::F(5)) => self.set_theme("base16-mocha.dark"),
            Event::Key(Key::F(6)) => self.set_theme("base16-ocean.dark"),
            Event::Key(Key::F(7)) => self.set_theme("base16-ocean.light"),
            Event::Key(Key::F(8)) => self.set_theme("Solarized (dark)"),
            Event::Key(Key::F(9)) => self.set_theme("Solarized (light)"),
            Event::Key(Key::F(10)) => self.set_theme("InspiredGitHub"),
            k => {
                if let Some(current_view) = self.current_view {
                    if let Some(view) = self.views.get_mut(&current_view) {
                        view.handle_input(k);
                    }
                }
            }
        }
    }

    pub fn set_theme(&mut self, theme: &str) {
        let future = self.client.set_theme(theme).map_err(|_| ());
        self.handle.spawn(future);
    }

    fn process_open_requests(&mut self) {
        if self.pending_open_requests.is_empty() {
            return;
        }

        info!("process pending open requests");

        let Xim {
            ref mut pending_open_requests,
            ref mut views,
            ref mut current_view,
            ref term_size,
            ..
        } = *self;

        let mut done = vec![];
        for (idx, task) in pending_open_requests.iter_mut().enumerate() {
            match task.poll() {
                Ok(Async::Ready((id, mut view))) => {
                    info!("open request succeeded for {}", &id);
                    done.push(idx);
                    view.resize(term_size.1);
                    views.insert(id.clone(), view);
                    *current_view = Some(id);
                }
                Ok(Async::NotReady) => continue,
                Err(e) => panic!("\"open\" task failed: {}", e),
            }
        }
        for idx in done.iter().rev() {
            pending_open_requests.remove(*idx);
        }

        if pending_open_requests.is_empty() {
            info!("no more pending open request");
        }
    }

    fn process_terminal_events(&mut self) {
        let mut new_size: Option<(u16, u16)> = None;
        loop {
            match self.term.poll() {
                Ok(Async::Ready(Some(event))) => match event {
                    TerminalEvent::Resize(size) => {
                        new_size = Some(size);
                    }
                    TerminalEvent::Input(input) => self.handle_input(input),
                },
                Ok(Async::Ready(None)) => {
                    error!("terminal stream shut down => exiting");
                    self.shutdown = true;
                }
                Ok(Async::NotReady) => break,
                Err(_) => {
                    error!("error while polling terminal stream => exiting");
                    self.shutdown = true;
                }
            }
        }
        if let Some(size) = new_size {
            if !self.shutdown {
                self.handle_resize(size);
            }
        }
    }

    fn process_core_events(&mut self) {
        loop {
            match self.events.poll() {
                Ok(Async::Ready(Some(event))) => {
                    self.dispatch_core_event(event);
                }
                Ok(Async::Ready(None)) => {
                    error!("core stdout shut down => panicking");
                    panic!("core stdout shut down");
                }
                Ok(Async::NotReady) => break,
                Err(_) => {
                    error!("error while polling core => panicking");
                    panic!("error while polling core");
                }
            }
        }
    }

    fn process_delayed_events(&mut self) {
        let delayed_events: Vec<CoreEvent> = self.delayed_events.drain(..).collect();
        for event in delayed_events {
            self.dispatch_core_event(event);
        }
    }

    fn render(&mut self) -> Result<()> {
        let Xim {
            ref mut views,
            ref mut term,
            ref current_view,
            ref styles,
            ..
        } = *self;
        if let Some(current_view) = *current_view {
            if let Some(view) = views.get_mut(&current_view) {
                view.render(term.stdout(), styles)?;
                if let Err(e) = term.stdout().flush() {
                    error!("failed to flush stdout: {}", e);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum CoreEvent {
    Update(Update),
    ScrollTo(ScrollTo),
    SetStyle(Style),
}

impl Future for Xim {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.process_open_requests();
        self.process_delayed_events();
        self.process_terminal_events();
        self.process_core_events();

        if let Err(e) = self.render() {
            log_error(&e);
        }

        if self.shutdown {
            Ok(Async::Ready(()))
        } else {
            Ok(Async::NotReady)
        }
    }
}

pub struct XimService(UnboundedSender<CoreEvent>);

impl XimService {
    fn send_core_event(&mut self, event: CoreEvent) -> ServerResult<()> {
        if let Err(e) = self.0.start_send(event) {
            let e = format!("failed to send core event to XIM: {}", e);
            error!("{}", e);
            return Box::new(future::err(e.into()));
        }
        match self.0.poll_complete() {
            Ok(_) => Box::new(future::ok(())),
            Err(e) => {
                let e = format!("failed to send core event to XIM: {}", e);
                Box::new(future::err(e.into()))
            }
        }
    }
}


impl Frontend for XimService {
    fn update(&mut self, update: Update) -> ServerResult<()> {
        self.send_core_event(CoreEvent::Update(update))
    }

    fn scroll_to(&mut self, scroll_to: ScrollTo) -> ServerResult<()> {
        self.send_core_event(CoreEvent::ScrollTo(scroll_to))
    }

    fn def_style(&mut self, style: Style) -> ServerResult<()> {
        self.send_core_event(CoreEvent::SetStyle(style))
    }
    fn available_plugins(&mut self, plugins: AvailablePlugins) -> ServerResult<()> {
        info!("Received available_plugins: {:?}",plugins);
        Box::new(future::ok(()))
    }
    fn update_cmds(&mut self, cmds: UpdateCmds) -> ServerResult<()> {
        info!("Received update_cmds: {:?}",cmds);
        Box::new(future::ok(()))
    }
    fn plugin_started(&mut self, plugin: PluginStarted) -> ServerResult<()> {
        info!("Received plugin_started: {:?}",plugin);
        Box::new(future::ok(()))
    }
    fn plugin_stoped(&mut self, plugin: PluginStoped) -> ServerResult<()> {
        info!("Received plugin_stoped: {:?}",plugin);
        Box::new(future::ok(()))
    }
    fn theme_changed(&mut self, theme: ThemeChanged) -> ServerResult<()> {
        info!("Received theme_changed: {:?}",theme);
        Box::new(future::ok(()))
    }
    fn config_changed(&mut self, cfg: ConfigChanged) -> ServerResult<()> {
        info!("Received config_changed: {:?}",cfg);
        Box::new(future::ok(()))
    }
}

pub struct XimServiceBuilder(UnboundedSender<CoreEvent>);

impl XimServiceBuilder {
    pub fn new() -> (Self, UnboundedReceiver<CoreEvent>) {
        let (tx, rx) = unbounded();
        (XimServiceBuilder(tx), rx)
    }
}

impl FrontendBuilder<XimService> for XimServiceBuilder {
    fn build(self, _client: Client) -> XimService {
        XimService(self.0)
    }
}
