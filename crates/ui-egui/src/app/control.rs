use super::{ControlRequest, PhotocraftApp, Receiver, gpu_canvas};

impl PhotocraftApp {
    /// Make the UI pass run for a control request without stealing the user's keyboard focus.
    pub(crate) fn raise_for_control(&mut self, ctx: &egui::Context) {
        if let Some(raise) = self.services.raise_without_focus.as_mut() {
            raise();
        }
        ctx.request_repaint();
    }

    /// Attach a control channel (requests arrive from a transport thread: TCP, stdin, tests).
    pub fn with_control(mut self, rx: Receiver<ControlRequest>) -> Self {
        self.control_rx = Some(rx);
        self
    }

    /// Close this frame's stage record (`frame_stages`): keep it if it was slow, and hand it to
    /// the stroke trace. `ui_ms` is this pass's UI build. A pass egui will run again
    /// (`request_discard`, such as a sizing pass) leaves the record open, so a frame is one
    /// record across all its passes.
    pub(crate) fn finish_frame(&mut self, ctx: &egui::Context, ui_ms: f64) {
        self.perf.stage.ui_ms += ui_ms;
        self.perf.stage.stroking |= self.live_stroke.is_some();
        if ctx.will_discard() {
            return;
        }
        let stages = std::mem::take(&mut self.perf.stage);
        let stages = self.slow_frames.finish(stages, gpu_canvas::now_ms(), self.perf.gpu, self.perf.gpu_ms);
        let info = &self.perf.gpu_info;
        self.stroke_trace.frame(&stages, stages.stroking, || {
            let gpu = format!("{} {}", info.backend, info.adapter).trim().to_string();
            if info.present.is_empty() { gpu } else { format!("{gpu}, {}", info.present) }
        });
    }

    pub(crate) fn drain_control(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.control_rx.take() else { return };
        while let Ok(req) = rx.try_recv() {
            let reply = req.reply.clone();
            if req.deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) || !req.dispatch.start() {
                let _ = reply.send(serde_json::json!({"ok": false, "error": crate::control::expired_error()}));
                continue;
            }
            // UI requests need a rendered pass, but must not activate the app for an agent.
            if req.method.starts_with("ui.") && req.method != "ui.focus" {
                self.raise_for_control(ctx);
            }
            match crate::control::handle(self, ctx, &req) {
                crate::control::Outcome::Done(v) => {
                    let _ = reply.send(v);
                }
                crate::control::Outcome::AfterInput => self.input_waiters.push(reply),
                crate::control::Outcome::AfterJob(job) => self.jobs.waiters.push((job, reply)),
                crate::control::Outcome::Screenshot { token, path } => {
                    // Admission is budgeted here, after this pass's completed captures were
                    // drained, so a just-finished request never causes a false rejection.
                    let settle = crate::screenshot::settle_ms(ctx.global_style().animation_time);
                    let document = self.document_identity();
                    if let Err((reply, e)) = self.captures.admit(token, path, reply, document, settle, crate::screenshot::now()) {
                        let _ = reply.send(serde_json::json!({"ok": false, "error": e}));
                    }
                }
            }
            // `ui.set` may have changed the edit target: the next request sees its colours.
            self.sync_mask_targets();
        }
        self.control_rx = Some(rx);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn raising_for_control_uses_the_host_hook_without_focusing_the_viewport() {
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_called = called.clone();
        let services = crate::Services {
            raise_without_focus: Some(Box::new(move || callback_called.store(true, std::sync::atomic::Ordering::Relaxed))),
            ..Default::default()
        };
        let mut app = crate::PhotocraftApp::new(photocraft_engine::Session::new(), services);
        let ctx = egui::Context::default();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| app.raise_for_control(ui.ctx()));

        assert!(called.load(std::sync::atomic::Ordering::Relaxed));
        let focused = output.viewport_output.get(&egui::ViewportId::ROOT).is_some_and(|output| output.commands.contains(&egui::ViewportCommand::Focus));
        assert!(!focused, "control UI requests must not activate the app");
    }
}
