use libscreenshot::WindowCaptureProvider;
use url::Url;
use notify;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Result, Watcher};
use std::io::{BufRead, BufReader, Seek};
use std::path::{self, Path};
use std::sync::mpsc::Receiver;
use std::sync::mpsc::{Sender, channel};
use std::thread;
use core::time::Duration;

use std::sync::{Arc, Condvar, Mutex};

/// We derive Deserialize/Serialize so we can persist app state on shutdown.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(default)] // if we add new fields, give them default values when deserializing old state
pub struct TemplateApp {
    #[serde(skip)]
    read_file_channel: (Sender<String>, Receiver<String>),
    read_file_text: String,
    #[serde(skip)]
    output_png_channel: (Sender<String>, Receiver<String>),
    output_png_file: String,
    label: String,
    #[serde(skip)]
    watch_channel: (Sender<Result<Event>>, Receiver<Result<Event>>),
    #[serde(skip)]
    outside_thread_controller: Arc<(Mutex<bool>, Condvar)>,
    #[serde(skip)]
    inside_thread_controller: Arc<(Mutex<bool>, Condvar)>,
    #[serde(skip)]
    outside_image_regen_marker: Arc<(Mutex<bool>, Condvar)>,
    #[serde(skip)]
    inside_image_regen_marker: Arc<(Mutex<bool>, Condvar)>,
    #[serde(skip)]
    state: Arc<Mutex<State>>,
}

struct State {
    duration: u64,
    ctx: Option<egui::Context>,
}

impl State {
    pub fn new() -> Self {
        Self {
            duration: 0,
            ctx: None,
        }
    }
}

impl Default for TemplateApp {
    fn default() -> Self {
        let outside_controller = Arc::new((Mutex::new(true), Condvar::new()));
        let inside_controller = Arc::clone(&outside_controller);
        let outside_marker = Arc::new((Mutex::new(true), Condvar::new()));
        let inside_marker = Arc::clone(&outside_marker);
        let state = Arc::new(Mutex::new(State::new()));
        Self {
            read_file_channel: channel(),
            read_file_text: "Path to log file to read".into(),
            output_png_channel: channel(),
            output_png_file: "seedshot.png".into(),
            watch_channel: channel(),
            outside_thread_controller: outside_controller,
            inside_thread_controller: inside_controller,
            outside_image_regen_marker: outside_marker,
            inside_image_regen_marker: inside_marker,
            // Example stuff:
            label: "Hello World!".to_owned(),
            state,
        }
    }
}

impl TemplateApp {
    /// Called once before the first frame.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // This is also where you can customize the look and feel of egui using
        // `cc.egui_ctx.set_visuals` and `cc.egui_ctx.set_fonts`.

        // Load previous app state (if any).
        // Note that you must enable the `persistence` feature for this to work.
        if let Some(storage) = cc.storage {
            eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default()
        } else {
            let app:  TemplateApp  = Default::default();
            app.state.lock().unwrap().ctx = Some(cc.egui_ctx.clone());
            app
        }
    }
}

impl eframe::App for TemplateApp {
    /// Called by the framework to save state before shutdown.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, self);
    }

    /// Called each time the UI needs repainting, which may be many times per second.
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Ok(text) = self.read_file_channel.1.try_recv() {
            self.read_file_text = text;
        }
        if let Ok(text) = self.output_png_channel.1.try_recv() {
            self.output_png_file = text;
        }
        // Put your widgets into a `SidePanel`, `TopBottomPanel`, `CentralPanel`, `Window` or `Area`.
        // For inspiration and more examples, go to https://emilk.github.io/egui

        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            // The top panel is often a good place for a menu bar:

            egui::MenuBar::new().ui(ui, |ui| {
                // NOTE: no File->Quit on web pages!
                let is_web = cfg!(target_arch = "wasm32");
                if !is_web {
                    ui.menu_button("File", |ui| {
                        if ui.button("Quit").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                    ui.add_space(16.0);
                }

                egui::widgets::global_theme_preference_buttons(ui);
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            // The central panel the region left after adding TopPanel's and SidePanel's
            ui.heading("LC Seedshotter");
            if ui.button("📂 Select Player.log file").clicked() {
                let sender = self.read_file_channel.0.clone();
                let task = rfd::AsyncFileDialog::new()
                    .add_filter("log", &["log"])
                    .pick_file();
                let ctx = ui.ctx().clone();
                execute(async move {
                    let file = task.await;
                    if let Some(file) = file {
                        let _ = sender.send(file.path().to_string_lossy().to_string());
                        ctx.request_repaint();
                    }
                });
            }
            ui.label(format!("Selected log file: {}", self.read_file_text));

            if ui.button("📂 Select screenshot output file").clicked() {
                let sender = self.output_png_channel.0.clone();
                let task = rfd::AsyncFileDialog::new()
                    .add_filter("png", &["png"])
                    .save_file();
                let ctx = ui.ctx().clone();
                execute(async move {
                    let file = task.await;
                    if let Some(file) = file {
                        let _ = sender.send(file.path().to_string_lossy().to_string());
                        ctx.request_repaint();
                    }
                });
            }
            ui.label(format!("Screenshot output file: {}", self.output_png_file));

            if ui.button("Start seedshotter").clicked() {
                let (lock, _cvar) = &*self.outside_thread_controller;
                let mut should_stop = lock.lock().unwrap();
                *should_stop = false;
                let watched_file = self.read_file_text.clone();
                let screenshot_file = self.output_png_file.clone();
                let inside_cond_var = self.inside_thread_controller.clone();
                let inside_image_marker = self.inside_image_regen_marker.clone();
                let state = Arc::new(Mutex::new(State::new()));
                state.lock().unwrap().ctx = Some(ctx.clone());
                self.state = state;
                let state_clone = self.state.clone();

                thread::spawn(move || {
                    run_seedshotter(watched_file, screenshot_file, inside_cond_var, inside_image_marker, state_clone).unwrap();
                });
            }
            if ui.button("Stop seedshotter").clicked() {
                let (lock, cvar) = &*self.outside_thread_controller;
                let mut should_stop = lock.lock().unwrap();
                *should_stop = true;
                cvar.notify_all();
            }

            let (lock, _cvar) = &*self.outside_thread_controller;
            let should_stop = lock.lock().unwrap();
            ui.label(format!("Seedshotter running: {}", !*should_stop));

            if Path::new(&self.output_png_file).exists() {
                let uri = Url::from_file_path(Path::new(&self.output_png_file)).unwrap();
                let (lock, cvar) = &*self.outside_image_regen_marker;
                let mut image_regenerated = lock.lock().unwrap();
                if *image_regenerated {
                    *image_regenerated = false;
                    cvar.notify_all();
                    ui.ctx().forget_image(&*uri.to_string());
                }
                ui.add(egui::Image::new(uri.to_string()).shrink_to_fit().corner_radius(5));
                // ui.add(egui::Image::new(uri.to_string()).shrink_to_fit().corner_radius(5));
                // ui.image(uri.to_string());
                // let image = egui::Image::new(uri.to_string()).shrink_to_fit().show_loading_spinner(true);
                // ui.add(image.);
                // let loaded_image = ui.ctx().try_load_image(uri.as_str(), egui::SizeHint::default()).unwrap();
                // ui.image(loaded_image.int);

            }
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                egui::warn_if_debug_build(ui);
            });
        });
    }
}

fn run_seedshotter(
    watched_file_path_string: String,
    screenshot_file_path_string: String,
    inside_shutdown_controller: Arc<(Mutex<bool>, Condvar)>,
    inside_image_marker: Arc<(Mutex<bool>, Condvar)>,
    state: Arc<Mutex<State>>,
) -> Result<()> {
    let provider = libscreenshot::get_window_capture_provider().expect("Unable to find provider");
    // Blatantly ripped from https://stackoverflow.com/a/76815714

    // get pos to end of file
    let watched_file_path = Path::new(&watched_file_path_string);
    let absolute_file_path = path::absolute(watched_file_path)?;

    let mut f = std::fs::File::open(watched_file_path)?;
    let mut pos = std::fs::metadata(watched_file_path)?.len();
    let (tx, rx) = channel();

    // set up watcher
    // Windows users get horrible polling because the terrible ReadDirectoryChangesW API REFUSES to
    // work.
    #[cfg(target_os = "windows")]
    let mut watcher = notify::PollWatcher::new(tx.clone(), Config::default().with_poll_interval(Duration::from_millis(10)))?;
    // let mut watcher = notify::windows::ReadDirectoryChangesWatcher::new(tx.clone(), Config::default())?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let mut watcher = RecommendedWatcher::new(tx.clone(), Config::default())?;

    watcher.watch(
        watched_file_path,
        RecursiveMode::NonRecursive,
    )?;
    thread::spawn(move || {
        let (lock, cvar) = &*inside_shutdown_controller;
        let mut should_stop = lock.lock().unwrap();
        while !*should_stop {
            should_stop = cvar.wait(should_stop).unwrap();
        }
        if !tx.send(Ok(Event::new(EventKind::Other))).is_ok() {
            println!("Unable to send event");
        }
    });
    println!(
        "Watching file {}",
        watched_file_path_string,
    );

    // watch
    for res in rx {
        match res {
            Ok(_event) => {
                println!("Event: {:?}", _event);
                if _event.kind == EventKind::Other {
                    println!("Closing seedshotter.");
                    break;
                }
                if path::absolute(_event.paths[0].clone())? != absolute_file_path {
                    continue;
                }
                // ignore any event that didn't change the pos
                if f.metadata()?.len() == pos {
                    continue;
                }

                // read from pos to end of file
                f.seek(std::io::SeekFrom::Start(pos))?;

                // update post to end of file
                pos = f.metadata()?.len();

                let reader = BufReader::new(&f);
                for line in reader.lines() {
                    if line?.contains("Players finished generating the new floor") {
                        let image = provider
                            .capture_focused_window()
                            .expect("Unable to capture focused window");
                        image
                            .save(&screenshot_file_path_string)
                            .expect("Unable to save image");
                        println!("Saving image to {}", screenshot_file_path_string);
                        let (lock, cvar) = &*inside_image_marker;
                        let mut image_regenerated = lock.lock()?;
                        *image_regenerated = true;
                        cvar.notify_all();
                        let ctx = &state.lock()?.ctx;
                        match ctx {
                            Some(x) => x.request_repaint(),
                            None => panic!("Error in Option<>"),
                        }
                    }
                }
            }
            Err(error) => println!("{error:?}"),
        }
    }

    Ok(())
}
#[cfg(not(target_arch = "wasm32"))]
fn execute<F: Future<Output = ()> + Send + 'static>(f: F) {
    // this is stupid... use any executor of your choice instead
    std::thread::spawn(move || futures::executor::block_on(f));
}

#[cfg(target_arch = "wasm32")]
fn execute<F: Future<Output = ()> + 'static>(f: F) {
    wasm_bindgen_futures::spawn_local(f);
}
