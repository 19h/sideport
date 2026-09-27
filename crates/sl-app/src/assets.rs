use gpui::{AssetSource, SharedString};
use std::borrow::Cow;

#[derive(Debug)]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let body = match path {
            "icons/check.svg" => "<path d='m5 12 4 4 10-10'/>",
            "icons/close.svg" | "icons/circle-x.svg" => "<path d='m6 6 12 12M18 6 6 18'/>",
            "icons/chevron-down.svg" => "<path d='m6 9 6 6 6-6'/>",
            "icons/chevron-up.svg" => "<path d='m6 15 6-6 6 6'/>",
            "icons/chevron-right.svg" => "<path d='m9 6 6 6-6 6'/>",
            "icons/chevron-left.svg" => "<path d='m15 6-6 6 6 6'/>",
            "icons/eye.svg" => "<path d='M2 12s4-7 10-7 10 7 10 7-4 7-10 7S2 12 2 12'/><circle cx='12' cy='12' r='3'/>",
            "icons/eye-off.svg" => "<path d='m3 3 18 18M10 5c7-1 12 7 12 7s-2 3-5 5M7 7c-3 2-5 5-5 5s4 7 10 7l3-1'/>",
            "icons/loader.svg" | "icons/loader-circle.svg" => "<path d='M21 12a9 9 0 1 1-9-9'/>",
            "icons/search.svg" => "<circle cx='10' cy='10' r='6'/><path d='m15 15 6 6'/>",
            "icons/dash.svg" | "icons/minus.svg" => "<path d='M5 12h14'/>",
            "icons/plus.svg" => "<path d='M12 5v14M5 12h14'/>",
            _ => return Ok(None),
        };
        let svg = format!(
            "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='none' stroke='black' stroke-width='2' stroke-linecap='round' stroke-linejoin='round'>{body}</svg>"
        );

        Ok(Some(Cow::Owned(svg.into_bytes())))
    }

    fn list(&self, _path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}
