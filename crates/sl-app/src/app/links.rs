//! Download sources: `sideloadly:` links and HTTP(S) IPA URLs, typed into the App section or
//! opened through the registered URL scheme.

use super::*;
use gpui_component::input::InputEvent;

impl Sideport {
    /// Download a link into the engine's downloads directory, then open the downloaded app.
    pub fn open_link(&mut self, link: String, window: &mut Window, cx: &mut Context<Self>) {
        let link = link.trim().to_owned();

        if self.occupied() {
            self.error = Some(format!("Finish the current operation before opening {link}."));
            cx.notify();
            return;
        }

        if link.is_empty() {
            return;
        }

        self.section = Section::App;
        self.app = None;
        self.icon = None;
        self.outcome = None;

        let job = self.engine.download(link);

        self.run_job(job, "Downloading…", window, cx, |view, path, window, cx| {
            // The job slot is released after this callback; open the download once it is.
            cx.defer_in(window, move |view, window, cx| view.load_path(path, window, cx));
            view.status = "Downloaded".into();
        });
    }

    /// Open each batch of URLs the system hands the app for as long as the view lives.
    pub fn listen_for_urls(
        &mut self,
        batches: async_channel::Receiver<Vec<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(urls) = batches.recv().await {
                if view.update_in(cx, |view, window, cx| view.open_urls(urls, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Open URLs handed to the app by the system: `sideloadly:` links, HTTP(S) URLs and files.
    pub fn open_urls(&mut self, urls: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(url) = urls.into_iter().next() else {
            return;
        };

        window.activate_window();

        match url.strip_prefix("file://") {
            Some(path) => {
                let path = url::Url::parse(&url).ok().and_then(|url| url.to_file_path().ok()).unwrap_or(path.into());
                self.load_path(path, window, cx);
            }
            None => self.open_link(url, window, cx),
        }
    }

    pub(super) fn submit_link_on_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.subscribe_in(&self.fields.link, window, |view, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                view.submit_link(window, cx);
            }
        })
        .detach();
    }

    pub(super) fn submit_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let link = self.fields.link.read(cx).value().to_string();

        self.open_link(link, window, cx);
    }
}
