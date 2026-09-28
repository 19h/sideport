//! Requests from other Sideport processes through the engine's local IPC server: a second launch
//! raises this window and hands over its file, and an updater asks the app to exit.

use super::*;
use sl_engine::ipc::IpcEvent;

impl Sideport {
    /// Handle IPC requests for as long as the view lives.
    pub fn listen_for_ipc(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let events = self.engine.subscribe_ipc();

        cx.spawn_in(window, async move |view, cx| {
            while let Ok(event) = events.recv().await {
                if view.update_in(cx, |view, window, cx| view.ipc_event(event, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn ipc_event(&mut self, event: IpcEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            IpcEvent::Raise { file } => {
                window.activate_window();
                cx.activate(true);

                if let Some(file) = file {
                    if self.occupied() {
                        self.error = Some(format!("Finish the current operation before opening {file}."));
                    } else {
                        self.load_path(file.into(), window, cx);
                    }
                }
            }

            IpcEvent::Restart { .. } => self.request_close(window, cx),

            IpcEvent::Enqueued { installation_id } => {
                let message = format!("Another Sideport process queued installation {installation_id} for refresh.");
                self.add_log(LogLevel::Info, message);
            }
        }

        cx.notify();
    }
}
