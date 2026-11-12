//! Opt-in accepted-input to reflecting-frame completion recorder for desktop journeys.
//! Pointer clocks start in `raw_input_hook` after the app has accepted an input event. Open/Save
//! clocks start when their control request is dispatched on the UI thread. They end from a
//! completion callback on the command buffer for the frame that consumed the final action.
//! This is GPU-work completion, not display scanout.

use serde_json::{Value, json};
use std::sync::{Arc, Mutex, mpsc::Sender};
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    records: Vec<Value>,
    waiters: Vec<(String, Sender<Value>)>,
}

/// Socket-thread read access. A read does not dispatch to the UI or request a repaint.
#[derive(Clone)]
pub struct Reader(Arc<Mutex<State>>);

impl Reader {
    pub fn wait(&self, params: &Value, reply: Sender<Value>) -> Result<(), &'static str> {
        let id = params["id"].as_str().ok_or("missing latency id")?;
        let mut state = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = state.records.iter().find(|r| r["id"] == id).ok_or("unknown latency id")?;
        if !record["completed_ns"].is_null() {
            let _ = reply.send(json!({"ok":true,"result":record}));
        } else if state.waiters.len() < 16 {
            state.waiters.push((id.into(), reply));
        } else {
            return Err("too many latency waiters");
        }
        Ok(())
    }
}

/// Constructed only when `PHOTOCRAFT_LATENCY_RECORDER=1`.
pub struct Recorder {
    epoch: Instant,
    state: Arc<Mutex<State>>,
    active: Option<Active>,
    queued: Vec<(String, u64)>,
}

struct Active {
    id: String,
    action: String,
    accepted: Option<u64>,
    command_completed: bool,
    input_completed: bool,
    input_completion_allowed: bool,
    test_delay_applied: bool,
    brush_dab_count: Option<usize>,
}

impl Recorder {
    pub fn from_env() -> Option<Self> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            (std::env::var("PHOTOCRAFT_LATENCY_RECORDER").as_deref() == Ok("1")).then(|| Self {
                epoch: Instant::now(),
                state: Arc::new(Mutex::new(State::default())),
                active: None,
                queued: Vec::new(),
            })
        }
        #[cfg(target_arch = "wasm32")]
        {
            None
        }
    }

    pub fn reader(&self) -> Reader {
        Reader(self.state.clone())
    }

    pub fn arm(&mut self, params: &Value) -> Result<(), &'static str> {
        let id = params["id"].as_str().filter(|s| !s.is_empty() && s.len() <= 128).ok_or("invalid latency id")?;
        let action = params["action"]
            .as_str()
            .filter(|s| matches!(*s, "brush" | "visibility" | "blend" | "move" | "zoom" | "pan" | "open" | "save"))
            .ok_or("invalid latency action")?;
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.active.is_some() || state.records.len() >= 4096 || state.records.iter().any(|r| r["id"] == id) {
            return Err("latency recorder busy, full, or duplicate id");
        }
        let frame_capture_requested = action == "brush" && std::env::var("PHOTOCRAFT_LATENCY_FRAME_CAPTURE").as_deref() == Ok("1");
        state.records.push(json!({"id":id,"action":action,"accepted_ns":null,"completed_ns":null,
            "duration_ns":null,"dab_count":null,"frame_pixel_hash":null,"frame_captures":[],"frame_capture_requested":frame_capture_requested,
            "endpoint":"accepted-input-frame-gpu-work-complete","input_samples":[]}));
        self.active = Some(Active {
            id: id.into(),
            action: action.into(),
            accepted: None,
            command_completed: false,
            input_completed: false,
            input_completion_allowed: action != "blend",
            test_delay_applied: false,
            brush_dab_count: None,
        });
        self.queued.clear();
        Ok(())
    }

    /// Optional physical slowdown dose for the calibration check. The accepted-input timestamp
    /// is recorded before this delay; the production path is inert unless both test variables are
    /// explicitly set in a recorder-enabled build.
    pub(crate) fn test_delay_after_accept(&mut self) -> Option<Duration> {
        let active = self.active.as_mut()?;
        if active.test_delay_applied || active.accepted.is_none() {
            return None;
        }
        let requested = std::env::var("PHOTOCRAFT_LATENCY_SLOW_ACTION").ok()?;
        if !test_delay_matches(&active.action, &active.id, &requested) {
            return None;
        }
        let millis = std::env::var("PHOTOCRAFT_LATENCY_SLOW_MS").ok()?.parse::<u64>().ok()?;
        if millis == 0 {
            return None;
        }
        active.test_delay_applied = true;
        Some(Duration::from_millis(millis))
    }

    /// Stamp an app.open/app.save control request on the UI thread after dispatch accepts it.
    pub(crate) fn accept_command(&mut self, action: &str) {
        let at = ns(self.epoch);
        let Some(active) = self.active.as_mut().filter(|active| active.action == action && active.accepted.is_none()) else { return };
        active.accepted = Some(at);
        self.queued.push(("command".into(), at));
    }

    /// Finish Open/Save only after PhotoCraft has applied the resulting document or written it.
    pub(crate) fn complete_command(&mut self, action: &str) {
        let at = ns(self.epoch);
        let Some(active) = self.active.as_mut().filter(|active| active.action == action && active.accepted.is_some()) else { return };
        active.command_completed = true;
        self.queued.push(("command-complete".into(), at));
    }

    /// A blend-mode UI action includes opening the dropdown and selecting an option. The driver
    /// marks the second pointer release as the semantic endpoint.
    pub(crate) fn set_input_completion_allowed(&mut self, allowed: bool) {
        if let Some(active) = self.active.as_mut().filter(|active| active.action == "blend") {
            active.input_completion_allowed = allowed;
        }
    }

    pub(crate) fn record_brush_dab_count(&mut self, count: usize) {
        if let Some(active) = self.active.as_mut().filter(|active| active.action == "brush") {
            active.brush_dab_count = Some(count);
        }
    }

    fn frame_capture(&self) -> Option<(String, Vec<(String, u64)>)> {
        let active = self.active.as_ref().filter(|active| active.action == "brush")?;
        let capture_requested = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .records
            .iter()
            .find(|record| record["id"] == active.id)
            .is_some_and(|record| record["frame_capture_requested"].as_bool() == Some(true));
        if !capture_requested {
            return None;
        }
        let events: Vec<_> = self
            .queued
            .iter()
            .filter(|(kind, _)| matches!(kind.as_str(), "down" | "move" | "up"))
            .cloned()
            .collect();
        (!events.is_empty()).then(|| (active.id.clone(), events))
    }

    /// Stamp events after the OS/egui host has placed them in this frame's input queue.
    pub(crate) fn accept_events(&mut self, events: &[egui::Event]) {
        if self.active.as_ref().is_none_or(|active| matches!(active.action.as_str(), "open" | "save")) {
            return;
        }
        for event in events {
            let kind = match event {
                egui::Event::PointerMoved(_) => "move",
                egui::Event::PointerButton { pressed: true, .. } => "down",
                egui::Event::PointerButton { pressed: false, .. } => "up",
                egui::Event::MouseWheel { .. } | egui::Event::Zoom(_) => "view",
                _ => continue,
            };
            let at = ns(self.epoch);
            if let Some(active) = self.active.as_mut() {
                if active.accepted.is_none() {
                    active.accepted = Some(at);
                }
                self.queued.push((kind.into(), at));
            }
        }
    }

    /// Called only after a visible UI pass. The callback is registered on that frame's GPU queue.
    pub(crate) fn frame_callback(&mut self, full_composite_complete: bool) -> Option<Box<dyn FnOnce() + Send>> {
        let active = self.active.as_mut()?;
        let id = active.id.clone();
        let brush_dab_count = active.brush_dab_count;
        let accepted = active.accepted?;
        let queued = std::mem::take(&mut self.queued);
        let epoch = self.epoch;
        let state = self.state.clone();
        let release = queued.iter().any(|(kind, _)| kind == "up");
        let view_step = queued.iter().any(|(kind, _)| kind == "view");
        let input_completed_now = release && (active.action != "blend" || active.input_completion_allowed);
        if input_completed_now {
            active.input_completed = true;
        }
        let input_completed = active.input_completed;
        let pending_completion = match active.action.as_str() {
            "open" => active.command_completed,
            "visibility" | "blend" => input_completed,
            _ => false,
        };
        if queued.is_empty() && !pending_completion {
            return None;
        }
        let finish = match active.action.as_str() {
            "brush" | "move" | "pan" => release,
            "visibility" | "blend" => input_completed && full_composite_complete,
            "zoom" => view_step,
            "open" => active.command_completed && full_composite_complete,
            "save" => active.command_completed,
            _ => false,
        };
        if finish {
            self.active = None;
        }
        Some(Box::new(move || {
            let end = ns(epoch);
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(record) = state.records.iter_mut().find(|r| r["id"] == id) else { return };
            let samples = record["input_samples"].as_array_mut();
            if let Some(samples) = samples {
                for (kind, start) in queued {
                    samples.push(json!({"kind":kind,"accepted_ns":start,"presented_frame_ns":end,"duration_ns":end.saturating_sub(start)}));
                }
            }
            if finish {
                record["accepted_ns"] = json!(accepted);
                record["completed_ns"] = json!(end);
                record["duration_ns"] = json!(end.saturating_sub(accepted));
                record["dab_count"] = json!(brush_dab_count);
                let result = record.clone();
                state.waiters.retain(|(wait_id, reply)| {
                    if *wait_id == id {
                        let _ = reply.send(json!({"ok":true,"result":result}));
                        false
                    } else {
                        true
                    }
                });
            }
        }))
    }
}

fn test_delay_matches(action: &str, id: &str, requested: &str) -> bool {
    requested == action || id.contains(requested)
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn frame(app: &mut crate::PhotocraftApp, ui: &egui::Ui, frame: &eframe::Frame) {
    use eframe::egui_wgpu::{self, wgpu};
    use std::sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::time::Duration;
    if ui.ctx().will_discard() {
        return;
    }
    let Some(recorder) = app.latency.as_mut() else { return };
    let Some(render) = frame.wgpu_render_state() else { return };
    let full_composite_complete = app.session.active().is_some_and(|st| {
        let pixels = u64::from(st.doc.size.width).saturating_mul(u64::from(st.doc.size.height));
        let refresh = app.perf.stage.refresh;
        let full_refresh = matches!(refresh, "full" | "gpu-full" | "rect" | "gpu-rect" | "background");
        full_refresh && pixels > 0 && app.perf.stage.refresh_px >= pixels && !app.redraws.contains_key(&st.doc.id)
    });
    // Do not consume the queued sample until there is a GPU queue to attach its completion
    // fence to. This also makes the recorder inert when PhotoCraft is running through its CPU
    // fallback path.
    let capture = recorder.frame_capture();
    let Some(done) = recorder.frame_callback(full_composite_complete) else { return };
    if let Some((id, events)) = capture {
        let events: Vec<_> = events.into_iter().map(|(kind, accepted_ns)| json!({"kind":kind,"accepted_ns":accepted_ns})).collect();
        let state = recorder.state.clone();
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |image| {
            let crop = frame_crop_rgb(&image);
            let hash = frame_crop_hash(&crop);
            let pixel_path = std::env::var_os("PHOTOCRAFT_LATENCY_FRAME_CAPTURE_DIR").and_then(|dir| {
                let first_event = events.first()?.get("accepted_ns")?.as_u64()?;
                let safe_id: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
                let path = std::path::PathBuf::from(dir).join(format!("{safe_id}-{first_event}.rgb"));
                std::fs::create_dir_all(path.parent()?).ok()?;
                std::fs::write(&path, &crop).ok()?;
                Some(path)
            });
            let mut state = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(record) = state.records.iter_mut().find(|record| record["id"] == id) else { return };
            let release_frame = events.iter().any(|event| event["kind"] == "up");
            if release_frame {
                record["frame_pixel_hash"] = json!(hash.clone());
            }
            if let Some(captures) = record["frame_captures"].as_array_mut() {
                captures.push(json!({"accepted_events":events,"pixel_hash":hash,"pixel_path":pixel_path.map(|path| path.to_string_lossy().to_string())}));
            }
            if !record["completed_ns"].is_null() {
                let result = record.clone();
                state.waiters.retain(|(wait_id, reply)| {
                    if *wait_id == id {
                        let _ = reply.send(json!({"ok":true,"result":result}));
                        false
                    } else {
                        true
                    }
                });
            }
        })));
    }
    type Work = Arc<AtomicBool>;
    static POLLER: OnceLock<mpsc::Sender<Work>> = OnceLock::new();
    let device = render.device.clone();
    let poller = POLLER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Work>();
        let _ = std::thread::Builder::new().name("photocraft-latency-fence".into()).spawn(move || {
            while let Ok(done) = rx.recv() {
                while !done.load(Ordering::Acquire) {
                    if device.poll(wgpu::PollType::Poll).is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_micros(100));
                }
            }
        });
        tx
    });
    ui.painter().add(egui_wgpu::Callback::new_paint_callback(ui.max_rect(), Fence { done: Mutex::new(Some(done)), poller: poller.clone() }));
}

/// Centre crop used by the journey's pixel proof, in RGB order.
fn frame_crop_rgb(image: &egui::ColorImage) -> Vec<u8> {
    let [width, height] = image.size;
    let (x0, x1) = (width * 476 / 1600, width * 836 / 1600);
    let (y0, y1) = (height * 354 / 1000, height * 714 / 1000);
    let mut rgb = Vec::with_capacity((x1 - x0) * (y1 - y0) * 3);
    for y in y0..y1 {
        for x in x0..x1 {
            rgb.extend_from_slice(&image.pixels[y * width + x].to_array()[..3]);
        }
    }
    rgb
}

/// FNV-1a over the crop, in RGB order.
fn frame_crop_hash(rgb: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in rgb {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn frame(_app: &mut crate::PhotocraftApp, _ui: &egui::Ui, _frame: &mut eframe::Frame) {}

#[cfg(not(target_arch = "wasm32"))]
struct Fence {
    done: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    poller: std::sync::mpsc::Sender<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl eframe::egui_wgpu::CallbackTrait for Fence {
    fn prepare(
        &self,
        _device: &eframe::egui_wgpu::wgpu::Device,
        _queue: &eframe::egui_wgpu::wgpu::Queue,
        _screen: &eframe::egui_wgpu::ScreenDescriptor,
        encoder: &mut eframe::egui_wgpu::wgpu::CommandEncoder,
        _resources: &mut eframe::egui_wgpu::CallbackResources,
    ) -> Vec<eframe::egui_wgpu::wgpu::CommandBuffer> {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        if let Some(callback) = self.done.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() {
            let done = Arc::new(AtomicBool::new(false));
            let worker_done = done.clone();
            encoder.on_submitted_work_done(move || {
                callback();
                worker_done.store(true, Ordering::Release);
            });
            let _ = self.poller.send(done);
        }
        vec![]
    }
    fn paint(
        &self,
        _info: eframe::egui::epaint::PaintCallbackInfo,
        _pass: &mut eframe::egui_wgpu::wgpu::RenderPass<'static>,
        _resources: &eframe::egui_wgpu::CallbackResources,
    ) {
    }
}

fn ns(epoch: Instant) -> u64 {
    u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recorder_starts_at_accepted_input_and_waits_for_frame_completion() {
        let mut r = Recorder { epoch: Instant::now(), state: Arc::new(Mutex::new(State::default())), active: None, queued: Vec::new() };
        r.arm(&json!({"id":"stroke-1","action":"brush"})).unwrap();
        r.accept_events(&[egui::Event::PointerButton {
            pos: egui::pos2(2.0, 2.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }]);
        let reader = r.reader();
        let (tx, rx) = std::sync::mpsc::channel();
        reader.wait(&json!({"id":"stroke-1"}), tx).unwrap();
        assert!(rx.try_recv().is_err());
        let done = r.frame_callback(false).unwrap();
        assert!(rx.try_recv().is_err());
        done();
        assert!(rx.try_recv().is_err(), "brush remains armed until release");
        r.accept_events(&[egui::Event::PointerButton {
            pos: egui::pos2(2.0, 2.0),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        r.frame_callback(false).unwrap()();
        let reply = rx.recv().unwrap();
        assert!(reply["result"]["duration_ns"].as_u64().unwrap() > 0);
        assert_eq!(reply["result"]["input_samples"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn click_actions_wait_for_release_and_open_waits_for_completion() {
        let mut r = Recorder { epoch: Instant::now(), state: Arc::new(Mutex::new(State::default())), active: None, queued: Vec::new() };
        r.arm(&json!({"id":"toggle-1","action":"visibility"})).unwrap();
        r.accept_events(&[egui::Event::PointerButton {
            pos: egui::pos2(4.0, 4.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        }]);
        let down = r.frame_callback(false).unwrap();
        down();
        assert!(r.active.is_some(), "a click begins on press and finishes on release");
        r.accept_events(&[egui::Event::PointerButton {
            pos: egui::pos2(4.0, 4.0),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        r.frame_callback(false).unwrap()();
        assert!(r.active.is_some(), "the layer composite has not completed yet");
        r.frame_callback(true).unwrap()();
        assert!(r.active.is_none());

        r.arm(&json!({"id":"open-1","action":"open"})).unwrap();
        let reader = r.reader();
        let (tx, rx) = std::sync::mpsc::channel();
        reader.wait(&json!({"id":"open-1"}), tx).unwrap();
        r.accept_command("open");
        r.frame_callback(false).unwrap()();
        assert!(rx.try_recv().is_err(), "dispatch acceptance is not document completion");
        r.complete_command("open");
        r.frame_callback(false).unwrap()();
        assert!(rx.try_recv().is_err(), "opening the document alone is not its full composite");
        r.frame_callback(true).unwrap()();
        assert!(rx.recv().unwrap()["result"]["duration_ns"].as_u64().unwrap() > 0);
    }

    #[test]
    fn blend_waits_for_the_layer_mode_selection_release() {
        let mut r = Recorder { epoch: Instant::now(), state: Arc::new(Mutex::new(State::default())), active: None, queued: Vec::new() };
        r.arm(&json!({"id":"blend-1","action":"blend"})).unwrap();
        let click =
            |pressed| egui::Event::PointerButton { pos: egui::pos2(8.0, 8.0), button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        r.set_input_completion_allowed(false);
        r.accept_events(&[click(true), click(false)]);
        r.frame_callback(false).unwrap()();
        assert!(r.active.is_some(), "opening the blend list does not finish the mode change");
        r.set_input_completion_allowed(true);
        r.accept_events(&[click(true), click(false)]);
        r.frame_callback(false).unwrap()();
        assert!(r.active.is_some(), "the selected mode has not reached a full composite yet");
        r.frame_callback(true).unwrap()();
        assert!(r.active.is_none());
    }

    #[test]
    fn slowdown_check_can_target_one_brush_size_label() {
        assert!(test_delay_matches("brush", "aa-0-slow-brush-30px-1", "brush-30px"));
        assert!(!test_delay_matches("brush", "aa-0-fast-brush-500px-1", "brush-30px"));
    }
}
