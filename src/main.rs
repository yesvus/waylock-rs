mod auth;
mod config;
mod render;

use std::{
    env,
    error::Error,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use auth::{AuthResult, authenticate};
use config::{Config, parse_args};
use render::{
    ButtonRect, LockSurfaceState, SurfaceContent, draw_surface, format_duration, local_clock,
    local_date,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            EventLoop,
            channel::{Event as ChannelEvent, Sender, channel},
            timer::{TimeoutAction, Timer},
        },
        calloop_wayland_source::WaylandSource,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym},
        pointer::{BTN_LEFT, PointerEvent, PointerEventKind, PointerHandler},
    },
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
};

pub(crate) struct App {
    qh: QueueHandle<Self>,
    conn: Connection,
    compositor_state: CompositorState,
    output_state: OutputState,
    registry_state: RegistryState,
    seat_state: SeatState,
    shm: Shm,
    session_lock: Option<SessionLock>,
    surfaces: Vec<LockSurfaceState>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    auth_sender: Sender<AuthResult>,
    config: Config,
    last_activity: Instant,
    password: String,
    auth_in_flight: bool,
    auth_error_until: Option<Instant>,
    monitors_off: bool,
    exit: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    let config = parse_args()?;
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue).insert(loop_handle.clone())?;

    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let output_state = OutputState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);
    let seat_state = SeatState::new(&globals, &qh);
    let shm = Shm::bind(&globals, &qh)?;
    let session_lock_state = SessionLockState::new(&globals, &qh);
    let session_lock = session_lock_state.lock(&qh)?;

    let mut app = App {
        qh: qh.clone(),
        conn: conn.clone(),
        compositor_state,
        output_state,
        registry_state,
        seat_state,
        shm,
        session_lock: Some(session_lock.clone()),
        surfaces: Vec::new(),
        keyboard: None,
        pointer: None,
        auth_sender: channel::<AuthResult>().0,
        config,
        last_activity: Instant::now(),
        password: String::new(),
        auth_in_flight: false,
        auth_error_until: None,
        monitors_off: false,
        exit: false,
    };

    for output in app.output_state.outputs() {
        let wl_surface = app.compositor_state.create_surface(&qh);
        let surface = session_lock.create_lock_surface(wl_surface, &output, &qh);
        app.surfaces.push(LockSurfaceState {
            surface,
            width: 0,
            height: 0,
            pool: None,
            buffer: None,
            button: ButtonRect::default(),
        });
    }

    let (auth_sender, auth_receiver) = channel::<AuthResult>();
    app.auth_sender = auth_sender;
    loop_handle.insert_source(auth_receiver, |event, _, app| {
        if let ChannelEvent::Msg(result) = event {
            app.auth_finished(result);
        }
    })?;

    loop_handle.insert_source(Timer::from_duration(Duration::from_secs(1)), |_, _, app| {
        app.tick();
        if app.exit {
            TimeoutAction::Drop
        } else {
            TimeoutAction::ToDuration(Duration::from_secs(1))
        }
    })?;

    while !app.exit {
        event_loop.dispatch(Duration::from_millis(250), &mut app)?;
    }

    Ok(())
}

impl App {
    fn tick(&mut self) {
        if self
            .auth_error_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.auth_error_until = None;
        }

        if !self.monitors_off && self.last_activity.elapsed().as_secs() >= self.config.off_after {
            self.power_off_monitors();
        }

        self.redraw_all();
    }

    fn redraw_all(&mut self) {
        let qh = self.qh.clone();
        let color = self.config.color;
        let clock = local_clock();
        let date = local_date();
        let remaining = self
            .config
            .off_after
            .saturating_sub(self.last_activity.elapsed().as_secs());
        let timer = format!("OFF IN {}", format_duration(remaining));
        let error = self.auth_error_until.is_some();
        let password_len = self.password.chars().count();

        for surface in &mut self.surfaces {
            if surface.width != 0 && surface.height != 0 {
                draw_surface(
                    surface,
                    &qh,
                    SurfaceContent {
                        background: color,
                        clock: &clock,
                        date: &date,
                        timer: &timer,
                        password_len,
                        show_error: error,
                        button_enabled: self.config.power_off_command.is_some(),
                    },
                );
            }
        }
    }

    fn auth_finished(&mut self, result: AuthResult) {
        self.auth_in_flight = false;
        match result {
            AuthResult::Success => {
                if let Some(session_lock) = self.session_lock.take() {
                    session_lock.unlock();
                    let _ = self.conn.roundtrip();
                }
                self.exit = true;
            }
            AuthResult::Failure => {
                self.password.clear();
                self.auth_error_until = Some(Instant::now() + Duration::from_secs(2));
                self.redraw_all();
            }
        }
    }

    fn note_activity(&mut self) {
        self.last_activity = Instant::now();
        self.monitors_off = false;
    }

    fn power_off_monitors(&mut self) {
        self.monitors_off = true;
        if let Some(command) = &self.config.power_off_command {
            if let Some((program, args)) = command.split_first() {
                let _ = Command::new(program).args(args).spawn();
            }
        }
    }

    fn submit_password(&mut self) {
        if self.auth_in_flight || self.password.is_empty() {
            return;
        }

        self.auth_in_flight = true;
        let password = std::mem::take(&mut self.password);
        let sender = self.auth_sender.clone();
        let username = env::var("USER").unwrap_or_else(|_| String::from("root"));
        let pam_service = self.config.pam_service.clone();

        thread::spawn(move || {
            let result = authenticate(username, password, pam_service);
            let _ = sender.send(result);
        });
    }

    fn handle_key(&mut self, event: KeyEvent) {
        self.note_activity();
        if event.keysym == Keysym::Return || event.keysym == Keysym::KP_Enter {
            self.submit_password();
            return;
        }
        if event.keysym == Keysym::BackSpace || event.keysym == Keysym::Delete {
            self.password.pop();
            self.redraw_all();
            return;
        }
        if event.keysym == Keysym::Escape {
            self.password.clear();
            self.redraw_all();
            return;
        }
        if self.auth_in_flight {
            return;
        }
        if let Some(text) = event.utf8 {
            for character in text.chars().filter(|character| !character.is_control()) {
                self.password.push(character);
            }
            self.redraw_all();
        }
    }
}

impl SessionLockHandler for App {
    fn locked(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _session_lock: SessionLock) {
        self.redraw_all();
    }

    fn finished(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _session_lock: SessionLock,
    ) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        session_lock_surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(surface) = self.surfaces.iter_mut().find(|surface| {
            surface.surface.wl_surface().id() == session_lock_surface.wl_surface().id()
        }) else {
            return;
        };

        let (width, height) = configure.new_size;
        if width == 0 || height == 0 {
            return;
        }

        let size = width as usize * height as usize * 4;
        let needs_pool = surface.pool.as_ref().is_none_or(|pool| pool.len() < size);
        if needs_pool {
            surface.pool = Some(SlotPool::new(size, &self.shm).expect("create Wayland SHM pool"));
            surface.buffer = None;
        }
        surface.width = width;
        surface.height = height;
        draw_surface(
            surface,
            qh,
            SurfaceContent {
                background: self.config.color,
                clock: &local_clock(),
                date: &local_date(),
                timer: &format!("OFF IN {}", format_duration(self.config.off_after)),
                password_len: 0,
                show_error: false,
                button_enabled: self.config.power_off_command.is_some(),
            },
        );
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            self.keyboard.take();
        }
        if capability == Capability::Pointer {
            self.pointer.take();
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
    }
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.handle_key(event);
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.handle_key(event);
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: smithay_client_toolkit::seat::keyboard::Modifiers,
        _raw_modifiers: smithay_client_toolkit::seat::keyboard::RawModifiers,
        _layout: u32,
    ) {
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        if !events.is_empty() {
            self.note_activity();
        }
        let mut clicked = false;
        for event in events {
            if let PointerEventKind::Press { button, .. } = event.kind {
                if button != BTN_LEFT {
                    continue;
                }
                if self
                    .surfaces
                    .iter()
                    .find(|surface| surface.surface.wl_surface() == &event.surface)
                    .is_some_and(|surface| {
                        surface.button.contains(event.position.0, event.position.1)
                    })
                {
                    clicked = true;
                }
            }
        }
        if clicked {
            self.power_off_monitors();
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

smithay_client_toolkit::delegate_compositor!(App);
smithay_client_toolkit::delegate_keyboard!(App);
smithay_client_toolkit::delegate_output!(App);
smithay_client_toolkit::delegate_pointer!(App);
smithay_client_toolkit::delegate_registry!(App);
smithay_client_toolkit::delegate_seat!(App);
smithay_client_toolkit::delegate_session_lock!(App);
smithay_client_toolkit::delegate_shm!(App);
wayland_client::delegate_noop!(App: ignore wl_buffer::WlBuffer);
