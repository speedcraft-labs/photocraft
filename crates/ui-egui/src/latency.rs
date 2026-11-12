//! Opt-in accepted-input to reflecting-frame completion recorder for desktop journeys.
//! Pointer clocks start in `raw_input_hook` after the app has accepted an input event. Open/Save
//! clocks start when their control request is dispatched on the UI thread. They end from a
//! completion callback on the command buffer for the frame that consumed the final action.
//! This is GPU-work completion, not display scanout.

use serde_json::{Value, json};
use std::sync::{Arc, Mutex, mpsc::Sender};
use std::time::Instant;

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
        state.records.push(json!({"id":id,"action":action,"accepted_ns":null,"completed_ns":null,
            "duration_ns":null,"endpoint":"accepted-input-frame-gpu-work-complete","input_samples":[]}));
        self.active = Some(Active { id: id.into(), action: action.into(), accepted: None, command_completed: false });
        self.queued.clear();
        Ok(())
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
    pub(crate) fn frame_callback(&mut self) -> Option<Box<dyn FnOnce() + Send>> {
        let active = self.active.as_ref()?;
        if self.queued.is_empty() {
            return None;
        }
        let id = active.id.clone();
        let accepted = active.accepted?;
        let queued = std::mem::take(&mut self.queued);
        let epoch = self.epoch;
        let state = self.state.clone();
        let release = queued.iter().any(|(kind, _)| kind == "up");
        let view_step = queued.iter().any(|(kind, _)| kind == "view");
        let finish = match active.action.as_str() {
            "brush" | "visibility" | "blend" | "move" | "pan" => release,
            "zoom" => view_step,
            "open" | "save" => active.command_completed,
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
    // Do not consume the queued sample until there is a GPU queue to attach its completion
    // fence to. This also makes the recorder inert when PhotoCraft is running through its CPU
    // fallback path.
    let Some(done) = recorder.frame_callback() else { return };
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
        let done = r.frame_callback().unwrap();
        assert!(rx.try_recv().is_err());
        done();
        assert!(rx.try_recv().is_err(), "brush remains armed until release");
        r.accept_events(&[egui::Event::PointerButton {
            pos: egui::pos2(2.0, 2.0),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        r.frame_callback().unwrap()();
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
        let down = r.frame_callback().unwrap();
        down();
        assert!(r.active.is_some(), "a click begins on press and finishes on release");
        r.accept_events(&[egui::Event::PointerButton {
            pos: egui::pos2(4.0, 4.0),
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]);
        r.frame_callback().unwrap()();
        assert!(r.active.is_none());

        r.arm(&json!({"id":"open-1","action":"open"})).unwrap();
        let reader = r.reader();
        let (tx, rx) = std::sync::mpsc::channel();
        reader.wait(&json!({"id":"open-1"}), tx).unwrap();
        r.accept_command("open");
        r.frame_callback().unwrap()();
        assert!(rx.try_recv().is_err(), "dispatch acceptance is not document completion");
        r.complete_command("open");
        r.frame_callback().unwrap()();
        assert!(rx.recv().unwrap()["result"]["duration_ns"].as_u64().unwrap() > 0);
    }
}
