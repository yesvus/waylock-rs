use std::{
    env,
    error::Error,
    process::Command,
    thread,
    time::{Duration, Instant},
};

use pam_client2::{
    Context as PamContext, Flag as PamFlag, conv_mock::Conversation as PamConversation,
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
    },
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};
use wayland_client::{
    Connection, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_buffer, wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface},
};

const DEFAULT_COLOR: u32 = 0x4B3F72;
const DEFAULT_OFF_AFTER: u64 = 600;

struct Config {
    color: u32,
    off_after: u64,
    power_off_command: Option<Vec<String>>,
}

enum AuthResult {
    Success,
    Failure,
}

struct LockSurfaceState {
    surface: SessionLockSurface,
    width: u32,
    height: u32,
    pool: Option<SlotPool>,
    buffer: Option<Buffer>,
}

struct App {
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
    auth_sender: Sender<AuthResult>,
    config: Config,
    started: Instant,
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
        auth_sender: channel::<AuthResult>().0,
        config,
        started: Instant::now(),
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

fn parse_args() -> Result<Config, Box<dyn Error>> {
    let mut args = env::args().skip(1);
    let mut config = Config {
        color: DEFAULT_COLOR,
        off_after: DEFAULT_OFF_AFTER,
        power_off_command: None,
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!(
                    "usage: waylock-rs [--color RRGGBB] [--off-after SECONDS] [--power-off-command PROGRAM [ARGS...]]"
                );
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("waylock-rs 0.1.0");
                std::process::exit(0);
            }
            "--color" => {
                let value = args.next().ok_or("--color requires RRGGBB")?;
                config.color = parse_color(&value)?;
            }
            "--off-after" => {
                config.off_after = args.next().ok_or("--off-after requires seconds")?.parse()?;
            }
            "--power-off-command" => {
                let program = args
                    .next()
                    .ok_or("--power-off-command requires a program")?;
                let mut command = vec![program];
                command.extend(args);
                config.power_off_command = Some(command);
                break;
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    Ok(config)
}

fn parse_color(value: &str) -> Result<u32, Box<dyn Error>> {
    let value = value.strip_prefix('#').unwrap_or(value);
    if value.len() != 6 {
        return Err("color must be exactly six hexadecimal digits".into());
    }
    Ok(u32::from_str_radix(value, 16)?)
}

impl App {
    fn tick(&mut self) {
        if self
            .auth_error_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.auth_error_until = None;
        }

        if !self.monitors_off && self.started.elapsed().as_secs() >= self.config.off_after {
            self.monitors_off = true;
            if let Some(command) = &self.config.power_off_command {
                if let Some((program, args)) = command.split_first() {
                    let _ = Command::new(program).args(args).spawn();
                }
            }
        }

        self.redraw_all();
    }

    fn redraw_all(&mut self) {
        let qh = self.qh.clone();
        let color = self.config.color;
        let clock = local_clock();
        let remaining = self
            .config
            .off_after
            .saturating_sub(self.started.elapsed().as_secs());
        let timer = format!("OFF IN {}", format_duration(remaining));
        let error = self.auth_error_until.is_some();
        let password_len = self.password.chars().count();

        for surface in &mut self.surfaces {
            if surface.width != 0 && surface.height != 0 {
                draw_surface(surface, &qh, color, &clock, &timer, password_len, error);
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

    fn submit_password(&mut self) {
        if self.auth_in_flight || self.password.is_empty() {
            return;
        }

        self.auth_in_flight = true;
        let password = std::mem::take(&mut self.password);
        let sender = self.auth_sender.clone();
        let username = env::var("USER").unwrap_or_else(|_| String::from("root"));

        thread::spawn(move || {
            let result = match PamContext::new(
                "swaylock",
                Some(username.as_str()),
                PamConversation::with_credentials(username.clone(), password),
            ) {
                Ok(mut context) => {
                    if context.authenticate(PamFlag::empty()).is_ok() {
                        AuthResult::Success
                    } else {
                        AuthResult::Failure
                    }
                }
                Err(_) => AuthResult::Failure,
            };
            let _ = sender.send(result);
        });
    }

    fn handle_key(&mut self, event: KeyEvent) {
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
            self.config.color,
            &local_clock(),
            &format!("OFF IN {}", format_duration(self.config.off_after)),
            0,
            false,
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

fn draw_surface(
    surface: &mut LockSurfaceState,
    qh: &QueueHandle<App>,
    background: u32,
    clock: &str,
    timer: &str,
    password_len: usize,
    show_error: bool,
) {
    let Some(pool) = surface.pool.as_mut() else {
        return;
    };
    let (width, height) = (surface.width as i32, surface.height as i32);
    let (buffer, canvas) = match surface.buffer.take() {
        Some(buffer) => match buffer.canvas(pool) {
            Some(canvas) => (buffer, canvas),
            None => match pool.create_buffer(width, height, width * 4, wl_shm::Format::Argb8888) {
                Ok((buffer, canvas)) => (buffer, canvas),
                Err(_) => return,
            },
        },
        None => match pool.create_buffer(width, height, width * 4, wl_shm::Format::Argb8888) {
            Ok((buffer, canvas)) => (buffer, canvas),
            Err(_) => return,
        },
    };

    let background_pixel = argb(background);
    for pixel in canvas.chunks_exact_mut(4) {
        pixel.copy_from_slice(&background_pixel.to_le_bytes());
    }

    let clock_scale = (surface.height / 120).clamp(4, 12);
    let label_scale = (clock_scale / 2).max(2);
    draw_centered(
        canvas,
        surface.width,
        surface.height,
        clock,
        surface.height / 2 - clock_scale * 10,
        clock_scale,
        0xFFFFFFFF,
    );
    draw_password_dots(
        canvas,
        surface.width,
        surface.height,
        password_len,
        surface.height / 2 + clock_scale * 4,
        clock_scale,
        0xFFFFFFFF,
    );
    draw_centered(
        canvas,
        surface.width,
        surface.height,
        timer,
        surface.height / 2 + clock_scale * 14,
        label_scale,
        0xFFD8D2E8,
    );
    if show_error {
        draw_centered(
            canvas,
            surface.width,
            surface.height,
            "WRONG PASSWORD",
            surface.height / 2 + clock_scale * 25,
            label_scale,
            0xFFFFA0A0,
        );
    }

    let _ = buffer.attach_to(surface.surface.wl_surface());
    surface
        .surface
        .wl_surface()
        .damage_buffer(0, 0, width, height);
    surface.surface.wl_surface().commit();
    surface.buffer = Some(buffer);
    let _ = qh;
}

fn draw_password_dots(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    count: usize,
    y: u32,
    scale: u32,
    color: u32,
) {
    if count == 0 {
        return;
    }
    let size = scale.max(3);
    let gap = size * 2;
    let total = count as u32 * size + (count.saturating_sub(1) as u32 * gap);
    let mut x = width.saturating_sub(total) / 2;
    for _ in 0..count {
        for dy in 0..size {
            for dx in 0..size {
                let px = x + dx;
                let py = y + dy;
                if px < width && py < height {
                    let offset = ((py * width + px) * 4) as usize;
                    canvas[offset..offset + 4].copy_from_slice(&color.to_le_bytes());
                }
            }
        }
        x += size + gap;
    }
}

fn argb(rgb: u32) -> u32 {
    0xFF00_0000 | rgb
}

fn draw_centered(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    text: &str,
    y: u32,
    scale: u32,
    color: u32,
) {
    let glyph_width = 5 * scale;
    let spacing = scale;
    let text_width = text
        .chars()
        .count()
        .saturating_mul((glyph_width + spacing) as usize)
        .saturating_sub(spacing as usize);
    let x = width.saturating_sub(text_width as u32) / 2;
    draw_text(canvas, width, height, text, x, y, scale, color);
}

fn draw_text(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    text: &str,
    x: u32,
    y: u32,
    scale: u32,
    color: u32,
) {
    let mut cursor = x;
    for character in text.chars() {
        if let Some(glyph) = glyph(character) {
            for (row, bits) in glyph.iter().enumerate() {
                for column in 0..5u32 {
                    if bits & (1 << (4 - column)) == 0 {
                        continue;
                    }
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let px = cursor + column * scale + dx;
                            let py = y + row as u32 * scale + dy;
                            if px < width && py < height {
                                let offset = ((py * width + px) * 4) as usize;
                                canvas[offset..offset + 4].copy_from_slice(&color.to_le_bytes());
                            }
                        }
                    }
                }
            }
        }
        cursor += 6 * scale;
    }
}

fn glyph(character: char) -> Option<[u8; 7]> {
    Some(match character {
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b00001, 0b00001, 0b11110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b11100,
        ],
        ':' => [
            0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b00100, 0b00000,
        ],
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110,
        ],
        'I' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b11111,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        ' ' => [0; 7],
        _ => return None,
    })
}

fn local_clock() -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut local = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe { libc::localtime_r(&now, &mut local) };
    format!(
        "{:02}:{:02}:{:02}",
        local.tm_hour, local.tm_min, local.tm_sec
    )
}

fn format_duration(seconds: u64) -> String {
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

smithay_client_toolkit::delegate_compositor!(App);
smithay_client_toolkit::delegate_keyboard!(App);
smithay_client_toolkit::delegate_output!(App);
smithay_client_toolkit::delegate_registry!(App);
smithay_client_toolkit::delegate_seat!(App);
smithay_client_toolkit::delegate_session_lock!(App);
smithay_client_toolkit::delegate_shm!(App);
wayland_client::delegate_noop!(App: ignore wl_buffer::WlBuffer);
