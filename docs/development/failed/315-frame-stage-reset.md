# Failed approach: recorder callback after frame-stage reset

This branch retains the initial recorder callback placement. It called `latency::frame` from the outer `eframe::App::ui` wrapper after `ui_frame` returned. `ui_frame` runs `finish_frame`, which takes and resets the recorded canvas refresh stage first. The recorder therefore saw no full-composite stage and could not finish open, visibility, or blend measurements; the first corrected-campaign open attempt timed out after 600 seconds.

The successful placement calls `latency::frame` from the draw path after `ui_body` and before `finish_frame` takes the stage, so the fence is attached to the UI pass that contains the refresh. This branch is retained to document the rejected placement and contains no optimization.

Evidence: failed acquisition reports under `/jobs/work/speedcraft-labs/keep/315-photocraft-latency/reports/` and the diagnosis handoffs on hyper issue #291 (comments 88–89).
