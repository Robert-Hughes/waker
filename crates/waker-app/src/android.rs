use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use android_activity::{
    AndroidApp, InputStatus, MainEvent, PollEvent,
    input::{
        ImeOptions, InputEvent, InputType, KeyAction, KeyMapChar, Keycode, MotionAction,
        TextInputAction, TextInputState, TextSpan,
    },
};
use egui::{
    Event, Key, Modifiers, PointerButton, Pos2, RawInput, TouchDeviceId, TouchId, TouchPhase,
};
use egui_wgpu::{RendererOptions, ScreenDescriptor, WgpuConfiguration, wgpu};
use tracing::{error, info, warn};

use super::{
    APP_NAME, AndroidTextField, AndroidTextInputSnapshot, WakerApp, android_copy_text, diagnostics,
};

struct RepaintSignal {
    deadline: Mutex<Option<Instant>>,
    waker: android_activity::AndroidAppWaker,
}

impl RepaintSignal {
    fn new(app: &AndroidApp) -> Arc<Self> {
        Arc::new(Self {
            deadline: Mutex::new(Some(Instant::now())),
            waker: app.create_waker(),
        })
    }

    fn request(&self, delay: Duration) {
        let deadline = Instant::now() + delay;
        let mut current = self.deadline.lock().expect("repaint deadline poisoned");
        if current.is_none_or(|value| deadline < value) {
            *current = Some(deadline);
        }
        drop(current);
        self.waker.wake();
    }

    fn timeout(&self) -> Option<Duration> {
        let deadline = *self.deadline.lock().expect("repaint deadline poisoned");
        deadline.map(|value| value.saturating_duration_since(Instant::now()))
    }

    fn take_if_due(&self) -> bool {
        let now = Instant::now();
        let mut deadline = self.deadline.lock().expect("repaint deadline poisoned");
        if deadline.is_some_and(|value| value <= now) {
            *deadline = None;
            true
        } else {
            false
        }
    }
}

#[derive(Default)]
struct AndroidInput {
    events: Vec<Event>,
    primary_touch_id: Option<i32>,
    modifiers: Modifiers,
    combining_accent: Option<char>,
    primary_pointer_pressed: bool,
}

impl AndroidInput {
    fn push_touch(&mut self, device_id: i32, pointer_id: i32, phase: TouchPhase, pos: Pos2) {
        self.events.push(Event::Touch {
            device_id: TouchDeviceId(device_id.max(0) as u64),
            id: TouchId(pointer_id.max(0) as u64),
            phase,
            pos,
            force: None,
        });
    }

    fn press_primary(&mut self, pointer_id: i32, pos: Pos2) {
        self.primary_pointer_pressed = true;
        self.primary_touch_id = Some(pointer_id);
        self.events.push(Event::PointerMoved(pos));
        self.events.push(Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed: true,
            modifiers: self.modifiers,
        });
    }

    fn move_primary(&mut self, pointer_id: i32, pos: Pos2) {
        if self.primary_touch_id == Some(pointer_id) {
            self.events.push(Event::PointerMoved(pos));
        }
    }

    fn release_primary(&mut self, pointer_id: i32, pos: Pos2) {
        if self.primary_touch_id == Some(pointer_id) {
            self.events.push(Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: self.modifiers,
            });
            self.events.push(Event::PointerGone);
            self.primary_touch_id = None;
        }
    }

    fn cancel_primary(&mut self) {
        if self.primary_touch_id.take().is_some() {
            self.events.push(Event::PointerGone);
        }
    }
}

#[derive(Default)]
struct AndroidIme {
    field: Option<AndroidTextField>,
    text: Option<String>,
    selection: Option<(usize, usize)>,
    compose_region: Option<TextSpan>,
}

impl AndroidIme {
    fn update(
        &mut self,
        app: &AndroidApp,
        snapshot: Option<&AndroidTextInputSnapshot>,
        user_tapped: bool,
    ) {
        let Some(snapshot) = snapshot else {
            if user_tapped && self.field.take().is_some() {
                tracing::debug!("hiding GameActivity IME after text focus moved away");
                app.hide_soft_input(false);
                self.text = None;
                self.selection = None;
                self.compose_region = None;
            }
            return;
        };

        let selection = (
            char_index_to_utf16(&snapshot.text, snapshot.selection_start),
            char_index_to_utf16(&snapshot.text, snapshot.selection_end),
        );
        let changed_field = self.field != Some(snapshot.field);
        let changed_state = changed_field
            || self.text.as_deref() != Some(snapshot.text.as_str())
            || self.selection != Some(selection);

        if changed_state {
            // GameTextInput_setState ultimately restarts Android's input
            // connection, so only call it when egui has genuinely changed the
            // editor state since the last state Android supplied.
            self.compose_region = None;
            tracing::debug!(
                field = ?snapshot.field,
                selection_start = selection.0,
                selection_end = selection.1,
                "synchronising egui editor state to GameTextInput"
            );
            app.set_text_input_state(TextInputState {
                text: snapshot.text.clone(),
                selection: TextSpan {
                    start: selection.0,
                    end: selection.1,
                },
                compose_region: None,
            });
            self.text = Some(snapshot.text.clone());
            self.selection = Some(selection);
        }

        if changed_field {
            app.set_ime_editor_info(
                InputType::TYPE_CLASS_TEXT,
                TextInputAction::Done,
                ImeOptions::IME_FLAG_NO_FULLSCREEN,
            );
            tracing::debug!(field = ?snapshot.field, "showing GameActivity IME for focused text field");
            app.show_soft_input(true);
        } else if snapshot.clicked {
            tracing::debug!(field = ?snapshot.field, "explicitly reopening GameActivity IME after text-field tap");
            app.show_soft_input(false);
        } else if user_tapped {
            tracing::debug!(
                field = ?snapshot.field,
                "ignoring non-text tap while text field retains focus"
            );
        }

        self.field = Some(snapshot.field);
    }

    fn accept_text_state(&mut self, state: &TextInputState) {
        self.text = Some(state.text.clone());
        self.selection = Some((state.selection.start, state.selection.end));
        self.compose_region = state.compose_region;
    }
}

fn char_index_to_utf16(text: &str, char_index: usize) -> usize {
    text.chars().take(char_index).map(char::len_utf16).sum()
}

fn utf16_index_to_char(text: &str, utf16_index: usize) -> usize {
    let mut units = 0;
    for (index, ch) in text.chars().enumerate() {
        if units >= utf16_index {
            return index;
        }
        units += ch.len_utf16();
        if units >= utf16_index {
            return index + 1;
        }
    }
    text.chars().count()
}

struct AndroidGpu {
    render_state: egui_wgpu::RenderState,
    surface: Option<wgpu::Surface<'static>>,
    surface_config: Option<wgpu::SurfaceConfiguration>,
}

impl AndroidGpu {
    fn new(app: &AndroidApp) -> Result<Self, String> {
        let native_window = app
            .native_window()
            .ok_or_else(|| "Android native window is unavailable".to_owned())?;
        let width = native_window.width().max(1) as u32;
        let height = native_window.height().max(1) as u32;
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = create_surface(&instance, &native_window)?;

        let render_state = pollster::block_on(egui_wgpu::RenderState::create(
            &WgpuConfiguration::default(),
            &instance,
            Some(&surface),
            RendererOptions::default(),
        ))
        .map_err(|error| format!("could not initialise Android wgpu: {error}"))?;

        let mut this = Self {
            render_state,
            surface: Some(surface),
            surface_config: None,
        };
        this.configure(width, height)?;
        Ok(this)
    }

    fn attach_window(&mut self, app: &AndroidApp) -> Result<(), String> {
        let native_window = app
            .native_window()
            .ok_or_else(|| "Android native window is unavailable".to_owned())?;
        let width = native_window.width().max(1) as u32;
        let height = native_window.height().max(1) as u32;
        self.surface = Some(create_surface(&self.render_state.instance, &native_window)?);
        self.configure(width, height)
    }

    fn detach_window(&mut self) {
        self.surface = None;
        self.surface_config = None;
    }

    fn resize_from_app(&mut self, app: &AndroidApp) -> Result<(), String> {
        let native_window = app
            .native_window()
            .ok_or_else(|| "Android native window is unavailable".to_owned())?;
        self.configure(
            native_window.width().max(1) as u32,
            native_window.height().max(1) as u32,
        )
    }

    fn configure(&mut self, width: u32, height: u32) -> Result<(), String> {
        let surface = self
            .surface
            .as_ref()
            .ok_or_else(|| "Android render surface is unavailable".to_owned())?;
        let mut config = surface
            .get_default_config(&self.render_state.adapter, width, height)
            .ok_or_else(|| "Android surface is incompatible with the selected GPU".to_owned())?;
        config.format = self.render_state.target_format;
        config.present_mode = wgpu::PresentMode::AutoVsync;
        config.desired_maximum_frame_latency = 1;
        surface.configure(&self.render_state.device, &config);
        self.surface_config = Some(config);
        Ok(())
    }

    fn dimensions(&self) -> Option<[u32; 2]> {
        self.surface_config
            .as_ref()
            .map(|config| [config.width, config.height])
    }

    fn paint(
        &mut self,
        android_app: &AndroidApp,
        ctx: &egui::Context,
        app: &mut WakerApp,
        raw_input: RawInput,
    ) -> Result<egui::FullOutput, String> {
        let output = ctx.run_ui(raw_input, |ui| app.show_ui(ui));
        let Some([width, height]) = self.dimensions() else {
            return Ok(output);
        };
        let Some(surface) = self.surface.as_ref() else {
            return Ok(output);
        };

        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.configure(width, height)?;
                return Ok(output);
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.attach_window(android_app)?;
                return Ok(output);
            }
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Validation => return Ok(output),
        };

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let paint_jobs = ctx.tessellate(output.shapes.clone(), output.pixels_per_point);
        let screen = ScreenDescriptor {
            size_in_pixels: [width, height],
            pixels_per_point: output.pixels_per_point,
        };

        let mut renderer = self.render_state.renderer.write();
        for (id, deltas) in &output.textures_delta.set {
            for delta in deltas {
                renderer.update_texture(
                    &self.render_state.device,
                    &self.render_state.queue,
                    *id,
                    delta,
                );
            }
        }

        let mut encoder =
            self.render_state
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("Waker Android egui encoder"),
                });
        let user_commands = renderer.update_buffers(
            &self.render_state.device,
            &self.render_state.queue,
            &mut encoder,
            &paint_jobs,
            &screen,
        );

        {
            let render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Waker Android egui pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            renderer.render(&mut render_pass.forget_lifetime(), &paint_jobs, &screen);
        }

        self.render_state.queue.submit(
            user_commands
                .into_iter()
                .chain(std::iter::once(encoder.finish())),
        );
        self.render_state.queue.present(frame);

        for id in &output.textures_delta.free {
            renderer.free_texture(id);
        }

        Ok(output)
    }
}

#[allow(unsafe_code)]
fn create_surface(
    instance: &wgpu::Instance,
    native_window: &impl raw_window_handle::HasWindowHandle,
) -> Result<wgpu::Surface<'static>, String> {
    let raw_window_handle = native_window
        .window_handle()
        .map_err(|error| format!("could not get Android window handle: {error}"))?
        .as_raw();
    unsafe {
        instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(raw_window_handle::AndroidDisplayHandle::new().into()),
            raw_window_handle,
        })
    }
    .map_err(|error| format!("could not create Android wgpu surface: {error}"))
}

fn native_pixels_per_point(app: &AndroidApp) -> f32 {
    app.config()
        .density()
        .map_or(1.0, |dpi| dpi as f32 / 160.0)
        .max(1.0)
}

fn make_raw_input(
    app: &mut WakerApp,
    ctx: &egui::Context,
    input: &mut AndroidInput,
    focused: bool,
    size_in_pixels: [u32; 2],
    started: Instant,
) -> RawInput {
    let native_ppp = app
        .android_app
        .as_ref()
        .map_or(1.0, native_pixels_per_point);
    let pixels_per_point = native_ppp * ctx.zoom_factor();
    let screen_rect = egui::Rect::from_min_size(
        Pos2::ZERO,
        egui::vec2(
            size_in_pixels[0] as f32 / pixels_per_point,
            size_in_pixels[1] as f32 / pixels_per_point,
        ),
    );

    let mut raw = RawInput {
        screen_rect: Some(screen_rect),
        time: Some(started.elapsed().as_secs_f64()),
        focused,
        events: std::mem::take(&mut input.events),
        ..Default::default()
    };
    if let Some(viewport) = raw.viewports.get_mut(&raw.viewport_id) {
        viewport.title = Some(APP_NAME.to_owned());
        viewport.native_pixels_per_point = Some(native_ppp);
        viewport.inner_rect = Some(screen_rect);
        viewport.focused = Some(focused);
    }
    app.apply_android_raw_input(ctx, &mut raw);
    raw
}

fn modifiers(meta: android_activity::input::MetaState) -> Modifiers {
    Modifiers {
        alt: meta.alt_on(),
        ctrl: meta.ctrl_on(),
        shift: meta.shift_on(),
        mac_cmd: false,
        command: meta.ctrl_on(),
    }
}

fn key_map_character(
    app: &AndroidApp,
    event: &android_activity::input::KeyEvent<'_>,
    combining_accent: &mut Option<char>,
) -> Option<char> {
    if event.device_id() == 0 {
        return None;
    }
    let map = match app.device_key_character_map(event.device_id()) {
        Ok(map) => map,
        Err(error) => {
            warn!(%error, device_id = event.device_id(), "could not load Android key character map");
            return None;
        }
    };

    match map.get(event.key_code(), event.meta_state()) {
        Ok(KeyMapChar::Unicode(ch)) => {
            if event.action() != KeyAction::Down {
                return Some(ch);
            }
            let combined = if let Some(accent) = combining_accent.take() {
                match map.get_dead_char(accent, ch) {
                    Ok(Some(combined)) => Some(combined),
                    Ok(None) => Some(ch),
                    Err(error) => {
                        warn!(%error, "could not combine Android dead key");
                        Some(ch)
                    }
                }
            } else {
                Some(ch)
            };
            combined
        }
        Ok(KeyMapChar::CombiningAccent(accent)) => {
            if event.action() == KeyAction::Down {
                *combining_accent = Some(accent);
            }
            None
        }
        Ok(KeyMapChar::None) => None,
        Err(error) => {
            warn!(%error, "could not map Android key character");
            *combining_accent = None;
            None
        }
    }
}

fn egui_key(key: Keycode) -> Option<Key> {
    match key {
        Keycode::DpadUp => Some(Key::ArrowUp),
        Keycode::DpadDown => Some(Key::ArrowDown),
        Keycode::DpadLeft => Some(Key::ArrowLeft),
        Keycode::DpadRight => Some(Key::ArrowRight),
        Keycode::Escape => Some(Key::Escape),
        Keycode::Tab => Some(Key::Tab),
        Keycode::Enter | Keycode::NumpadEnter => Some(Key::Enter),
        Keycode::Space => Some(Key::Space),
        Keycode::Del => Some(Key::Backspace),
        Keycode::ForwardDel => Some(Key::Delete),
        Keycode::MoveHome => Some(Key::Home),
        Keycode::MoveEnd => Some(Key::End),
        Keycode::PageUp => Some(Key::PageUp),
        Keycode::PageDown => Some(Key::PageDown),
        _ => None,
    }
}

fn drain_input(
    app: &AndroidApp,
    state: &mut AndroidInput,
    ime: &mut AndroidIme,
    waker_app: &mut WakerApp,
    pixels_per_point: f32,
) {
    let Ok(mut iter) = app.input_events_iter() else {
        return;
    };

    while iter.next(|event| match event {
        InputEvent::MotionEvent(event) => {
            let device_id = event.device_id();
            match event.action() {
                MotionAction::Down | MotionAction::PointerDown => {
                    let pointer = event.pointer_at_index(event.pointer_index());
                    let id = pointer.pointer_id();
                    let pos = Pos2::new(
                        pointer.x() / pixels_per_point,
                        pointer.y() / pixels_per_point,
                    );
                    state.push_touch(device_id, id, TouchPhase::Start, pos);
                    if state.primary_touch_id.is_none() {
                        state.press_primary(id, pos);
                    }
                }
                MotionAction::Move => {
                    for pointer in event.pointers() {
                        let id = pointer.pointer_id();
                        let pos = Pos2::new(
                            pointer.x() / pixels_per_point,
                            pointer.y() / pixels_per_point,
                        );
                        state.push_touch(device_id, id, TouchPhase::Move, pos);
                        state.move_primary(id, pos);
                    }
                }
                MotionAction::Up | MotionAction::PointerUp => {
                    let pointer = event.pointer_at_index(event.pointer_index());
                    let id = pointer.pointer_id();
                    let pos = Pos2::new(
                        pointer.x() / pixels_per_point,
                        pointer.y() / pixels_per_point,
                    );
                    state.push_touch(device_id, id, TouchPhase::End, pos);
                    state.release_primary(id, pos);
                }
                MotionAction::Cancel => {
                    for pointer in event.pointers() {
                        let id = pointer.pointer_id();
                        let pos = Pos2::new(
                            pointer.x() / pixels_per_point,
                            pointer.y() / pixels_per_point,
                        );
                        state.push_touch(device_id, id, TouchPhase::Cancel, pos);
                    }
                    state.cancel_primary();
                }
                _ => {}
            }
            InputStatus::Handled
        }
        InputEvent::KeyEvent(event) => {
            if event.key_code() == Keycode::Back {
                return InputStatus::Unhandled;
            }

            state.modifiers = modifiers(event.meta_state());
            state.events.push(Event::ModifiersChanged(state.modifiers));

            let mapped_character = key_map_character(app, event, &mut state.combining_accent);
            let key = egui_key(event.key_code())
                .or_else(|| mapped_character.and_then(|ch| Key::from_name(&ch.to_string())));
            let pressed = event.action() == KeyAction::Down;

            if let Some(key) = key {
                state.events.push(Event::Key {
                    key,
                    physical_key: None,
                    pressed,
                    repeat: pressed && event.repeat_count() > 0,
                    modifiers: state.modifiers,
                });
            }

            if pressed
                && !state.modifiers.ctrl
                && !state.modifiers.command
                && !state.modifiers.mac_cmd
                && let Some(ch) = mapped_character
                && !ch.is_control()
            {
                state.events.push(Event::Text(ch.to_string()));
            }

            if key.is_some() || mapped_character.is_some() {
                InputStatus::Handled
            } else {
                InputStatus::Unhandled
            }
        }
        InputEvent::TextEvent(text_state) => {
            ime.accept_text_state(text_state);
            let selection_start = utf16_index_to_char(&text_state.text, text_state.selection.start);
            let selection_end = utf16_index_to_char(&text_state.text, text_state.selection.end);
            waker_app.apply_android_text_input_state(
                text_state.text.clone(),
                selection_start,
                selection_end,
            );
            InputStatus::Handled
        }
        InputEvent::TextAction(_) => {
            state.events.push(Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: state.modifiers,
            });
            state.events.push(Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: state.modifiers,
            });
            InputStatus::Handled
        }
        _ => InputStatus::Unhandled,
    }) {}
}

fn handle_platform_output(
    app: &AndroidApp,
    waker_app: &WakerApp,
    output: &egui::PlatformOutput,
    ime: &mut AndroidIme,
    user_tapped: bool,
) {
    ime.update(app, waker_app.android_text_input.as_ref(), user_tapped);

    for command in &output.commands {
        match command {
            egui::OutputCommand::CopyText(text) => {
                if let Err(error) = android_copy_text(app, text) {
                    warn!(%error, "could not copy egui text to Android clipboard");
                }
            }
            egui::OutputCommand::CopyImage(_) | egui::OutputCommand::OpenUrl(_) => {}
        }
    }
}

#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub fn android_main(android_app: AndroidApp) {
    let internal_data_path = android_app.internal_data_path();
    let default_config_path = internal_data_path
        .as_ref()
        .map(|path| path.join("waker.local.conf"));
    let diagnostics_runtime = diagnostics::init_android(internal_data_path);
    let diagnostics_info = diagnostics_runtime.info().clone();
    info!(
        persistent_logging = diagnostics_runtime.is_persistent(),
        log_dir = ?diagnostics_info.log_dir,
        warning = ?diagnostics_info.warning,
        "Waker starting with direct android-activity backend"
    );

    let ctx = egui::Context::default();
    let repaint = RepaintSignal::new(&android_app);
    let repaint_callback = Arc::clone(&repaint);
    ctx.set_request_repaint_callback(move |request| {
        repaint_callback.request(request.delay);
    });

    let mut waker_app = WakerApp::new(diagnostics_info, default_config_path);
    waker_app.android_app = Some(android_app.clone());
    waker_app.install_branding(&ctx);

    let started = Instant::now();
    let mut gpu: Option<AndroidGpu> = None;
    let mut input = AndroidInput::default();
    let mut focused = false;
    let mut input_available = false;
    let mut running = true;
    let mut ime = AndroidIme::default();

    while running {
        let timeout = repaint.timeout();
        android_app.poll_events(timeout, |event| match event {
            PollEvent::Main(MainEvent::Destroy) => {
                info!("Android GameActivity destroyed");
                gpu = None;
                running = false;
            }
            PollEvent::Main(MainEvent::InitWindow { .. }) => {
                let result = match gpu.as_mut() {
                    Some(gpu) => gpu.attach_window(&android_app),
                    None => AndroidGpu::new(&android_app).map(|created| {
                        gpu = Some(created);
                    }),
                };
                if let Err(error) = result {
                    error!(%error, "could not attach Android render surface");
                }
                repaint.request(Duration::ZERO);
            }
            PollEvent::Main(MainEvent::TerminateWindow { .. }) => {
                if let Some(gpu) = gpu.as_mut() {
                    gpu.detach_window();
                }
            }
            PollEvent::Main(MainEvent::WindowResized { .. }) => {
                if let Some(gpu) = gpu.as_mut()
                    && let Err(error) = gpu.resize_from_app(&android_app)
                {
                    error!(%error, "could not resize Android render surface");
                }
                repaint.request(Duration::ZERO);
            }
            PollEvent::Main(MainEvent::RedrawNeeded { .. })
            | PollEvent::Main(MainEvent::ContentRectChanged { .. })
            | PollEvent::Main(MainEvent::InsetsChanged { .. })
            | PollEvent::Main(MainEvent::ConfigChanged { .. })
            | PollEvent::Main(MainEvent::Resume { .. })
            | PollEvent::Main(MainEvent::Start) => {
                repaint.request(Duration::ZERO);
            }
            PollEvent::Main(MainEvent::InputAvailable) => {
                input_available = true;
            }
            PollEvent::Main(MainEvent::GainedFocus) => {
                focused = true;
                repaint.request(Duration::ZERO);
            }
            PollEvent::Main(MainEvent::LostFocus) => {
                focused = false;
                repaint.request(Duration::ZERO);
            }
            PollEvent::Main(MainEvent::Pause | MainEvent::Stop | MainEvent::SaveState { .. })
            | PollEvent::Main(MainEvent::LowMemory)
            | PollEvent::Wake
            | PollEvent::Timeout => {}
            _ => {}
        });

        if !running {
            break;
        }

        let native_ppp = native_pixels_per_point(&android_app);
        let pixels_per_point = native_ppp * ctx.zoom_factor();
        if input_available {
            drain_input(
                &android_app,
                &mut input,
                &mut ime,
                &mut waker_app,
                pixels_per_point,
            );
            input_available = false;
            repaint.request(Duration::ZERO);
        }

        if repaint.take_if_due()
            && let Some(gpu) = gpu.as_mut()
            && let Some(size) = gpu.dimensions()
        {
            let user_tapped = std::mem::take(&mut input.primary_pointer_pressed);
            let raw_input =
                make_raw_input(&mut waker_app, &ctx, &mut input, focused, size, started);
            match gpu.paint(&android_app, &ctx, &mut waker_app, raw_input) {
                Ok(output) => handle_platform_output(
                    &android_app,
                    &waker_app,
                    &output.platform_output,
                    &mut ime,
                    user_tapped,
                ),
                Err(error) => {
                    error!(%error, "Android egui render failed");
                }
            }
        }
    }

    info!("Waker Android activity loop exited");
}
