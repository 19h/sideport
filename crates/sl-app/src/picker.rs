use futures::future::LocalBoxFuture;
use gpui::{App, Window};
use std::path::{Path, PathBuf};

/// macOS uses an owned, window-attached panel; tests use GPUI's simulated platform.
pub(crate) fn paths(
    window: &Window,
    cx: &App,
    title: &'static str,
    multiple: bool,
) -> LocalBoxFuture<'static, anyhow::Result<Option<Vec<PathBuf>>>> {
    #[cfg(all(target_os = "macos", not(test)))]
    {
        let _ = cx;
        let dialog = rfd::AsyncFileDialog::new().set_parent(window).set_title(title);

        Box::pin(async move {
            let selected = if multiple {
                dialog.pick_files_or_folders().await
            } else {
                dialog.pick_file_or_folder().await.map(|path| vec![path])
            };

            Ok(selected.map(|paths| paths.into_iter().map(|path| path.path().to_owned()).collect()))
        })
    }

    #[cfg(any(not(target_os = "macos"), test))]
    {
        let _ = window;
        let selected = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: true,
            multiple,
            prompt: Some(title.into()),
        });

        Box::pin(async move { selected.await? })
    }
}

pub(crate) fn destination(
    window: &Window,
    cx: &App,
    directory: &Path,
    name: &str,
) -> LocalBoxFuture<'static, anyhow::Result<Option<PathBuf>>> {
    #[cfg(all(target_os = "macos", not(test)))]
    {
        let _ = cx;
        let dialog = rfd::AsyncFileDialog::new()
            .set_parent(window)
            .set_title("Export IPA")
            .set_directory(directory)
            .set_file_name(name);

        Box::pin(async move { Ok(dialog.save_file().await.map(|path| path.path().to_owned())) })
    }

    #[cfg(any(not(target_os = "macos"), test))]
    {
        let _ = window;
        let selected = cx.prompt_for_new_path(directory, Some(name));

        Box::pin(async move { selected.await? })
    }
}
